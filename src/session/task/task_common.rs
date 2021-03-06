//! This file contains all methods that are:
//! (a) Common between ReplayTask and Record tasks. These methods are called from forwarding stubs
//!     in the trait impls. These stubs are needed because default methods in the trait
//!     implementation have an implicit ?Sized constraint. By calling the stubs that call the
//!     methods in this file we get Sized for "free" because both ReplayTask and RecordTask are
//!     Sized.
//! (b) Some utility methods which because of their template parameters cannot be added to the
//!     Task trait. This makes calling them a tad bit more inconvenient as we _cannot_ invoke using
//!     the self.func_name() style. They are included in this file because they take &dyn Task or
//!     &mut dyn Task as their first parameter. It would have been confusing to include them
//!     in task_inner.rs
//! (c) Some misc methods that did not fit elsewhere...

use crate::{
    arch::Architecture,
    auto_remote_syscalls::{AutoRemoteSyscalls, AutoRestoreMem},
    bindings::{
        kernel::{
            user_desc,
            user_regs_struct as native_user_regs_struct,
            NT_FPREGSET,
            NT_PRSTATUS,
            NT_X86_XSTATE,
            SHMDT,
        },
        prctl::{ARCH_GET_FS, ARCH_GET_GS, ARCH_SET_FS, ARCH_SET_GS},
        ptrace::{
            PTRACE_ARCH_PRCTL,
            PTRACE_DETACH,
            PTRACE_EVENT_EXIT,
            PTRACE_GETREGS,
            PTRACE_GETSIGINFO,
            PTRACE_POKEUSER,
            PTRACE_SETFPREGS,
            PTRACE_SETFPXREGS,
            PTRACE_SETREGS,
            PTRACE_SETREGSET,
        },
        signal::POLL_IN,
    },
    core::type_has_no_holes,
    extra_registers::{ExtraRegisters, Format},
    fast_forward::at_x86_string_instruction,
    file_monitor,
    kernel_abi::{
        common::{
            preload_interface,
            preload_interface::{preload_globals, syscallbuf_hdr, syscallbuf_record},
        },
        is_at_syscall_instruction,
        is_mprotect_syscall,
        syscall_instruction_length,
        syscall_number_for_arch_prctl,
        syscall_number_for_close,
        syscall_number_for_mprotect,
        syscall_number_for_munmap,
        syscall_number_for_openat,
        x64,
        x86,
        CloneTLSType,
        FcntlOperation,
        SupportedArch,
    },
    kernel_metadata::{ptrace_req_name, signal_name},
    kernel_supplement::ARCH_SET_CPUID,
    log::LogLevel::{LogDebug, LogInfo, LogWarn},
    perf_counters::TIME_SLICE_SIGNAL,
    rd::RD_RESERVED_ROOT_DIR_FD,
    registers::{with_converted_registers, Registers, X86_TF_FLAG},
    remote_code_ptr::RemoteCodePtr,
    remote_ptr::{RemotePtr, Void},
    scoped_fd::ScopedFd,
    seccomp_filter_rewriter::SECCOMP_MAGIC_SKIP_ORIGINAL_SYSCALLNO,
    session::{
        address_space::{
            address_space::AddressSpace,
            kernel_mapping::KernelMapping,
            memory_range::MemoryRangeKey,
            BreakpointType,
            DebugStatus,
        },
        session_inner::session_inner::SessionInner,
        task::{
            is_signal_triggered_by_ptrace_interrupt,
            is_singlestep_resume,
            task_inner::{
                task_inner::{CapturedState, CloneReason, PtraceData, WriteFlags},
                CloneFlags,
                ResumeRequest,
                TicksRequest,
                TrapReasons,
                WaitRequest,
                MAX_TICKS_REQUEST,
            },
            Task,
            TaskSharedPtr,
            PRELOAD_THREAD_LOCALS_SIZE,
        },
        Session,
    },
    ticks::Ticks,
    util::{
        ceil_page_size,
        cpuid,
        floor_page_size,
        is_kernel_trap,
        pwrite_all_fallible,
        trapped_instruction_at,
        trapped_instruction_len,
        u8_raw_slice,
        u8_raw_slice_mut,
        xsave_layout_from_trace,
        xsave_native_layout,
        TrappedInstruction,
        XSaveLayout,
        CPUID_GETFEATURES,
    },
    wait_status::WaitStatus,
};
use file_monitor::LazyOffset;
use libc::{
    pid_t,
    pread64,
    waitpid,
    CLONE_FILES,
    ECHILD,
    EPERM,
    ESRCH,
    PR_SET_NAME,
    PR_SET_SECCOMP,
    SECCOMP_MODE_FILTER,
    SIGKILL,
    SIGTRAP,
    WNOHANG,
    __WALL,
};
use nix::{
    errno::{errno, Errno},
    fcntl::OFlag,
    sys::mman::{MapFlags, ProtFlags},
};
use std::{
    cell::RefCell,
    cmp::min,
    convert::TryInto,
    ffi::{c_void, CStr, CString, OsStr},
    mem::{size_of, size_of_val, zeroed},
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr,
    rc::Rc,
    slice,
};

/// Forwarded method definition
///
/// Open /proc/{tid}/mem fd for our AddressSpace, closing the old one
/// first. If necessary we force the tracee to open the file
/// itself and smuggle the fd back to us.
/// Returns false if the process no longer exists.
pub(super) fn open_mem_fd<T: Task>(task: &mut T) -> bool {
    // Use ptrace to read/write during open_mem_fd
    task.vm().set_mem_fd(ScopedFd::new());

    if !task.is_stopped {
        log!(
            LogWarn,
            "Can't retrieve mem fd for {}; process not stopped, racing with exec?",
            task.tid
        );
        return false;
    }

    // We could try opening /proc/<pid>/mem directly first and
    // only do this dance if that fails. But it's simpler to
    // always take this path, and gives better test coverage. On Ubuntu
    // the child has to open its own mem file (unless rr is root).
    let path = CStr::from_bytes_with_nul(b"/proc/self/mem\0").unwrap();

    let arch = task.arch();
    let mut remote = AutoRemoteSyscalls::new(task);
    let remote_fd: i32;
    {
        let mut remote_path: AutoRestoreMem = AutoRestoreMem::push_cstr(&mut remote, path);
        if remote_path.get().is_some() {
            let remote_arch = remote_path.arch();
            let remote_addr = remote_path.get().unwrap();

            // AutoRestoreMem DerefMut-s to AutoRemoteSyscalls
            // skip leading '/' since we want the path to be relative to the root fd
            remote_fd = rd_syscall!(
                remote_path,
                syscall_number_for_openat(remote_arch),
                RD_RESERVED_ROOT_DIR_FD,
                // Skip the leading '/' in the path as this is a relative path.
                (remote_addr + 1usize).as_usize(),
                libc::O_RDWR
            ) as i32;
        } else {
            remote_fd = -ESRCH;
        }
    }
    let mut fd: ScopedFd = ScopedFd::new();
    if remote_fd != -ESRCH {
        if remote_fd < 0 {
            // This can happen when a process fork()s after setuid; it can no longer
            // open its own /proc/self/mem. Hopefully we can read the child's
            // mem file in this case (because rd is probably running as root).
            let buf: String = format!("/proc/{}/mem", remote.task().tid);
            fd = ScopedFd::open_path(Path::new(&buf), OFlag::O_RDWR);
        } else {
            fd = rd_arch_function!(remote, retrieve_fd_arch, arch, remote_fd);
            // Leak fd if the syscall fails due to the task being SIGKILLed unexpectedly
            rd_syscall!(remote, syscall_number_for_close(remote.arch()), remote_fd);
        }
    }
    if !fd.is_open() {
        log!(
            LogInfo,
            "Can't retrieve mem fd for {}; process no longer exists?",
            remote.task().tid
        );
        return false;
    }
    remote.task().vm().set_mem_fd(fd.try_into().unwrap());
    true
}

/// Forwarded method definition
///
/// Read/write the number of bytes.
/// Number of bytes read can be less than desired
/// - Returns Err(()) if No bytes could be read at all AND there was an error
/// - Returns Ok(usize) if 0 or more bytes could be read. All bytes requested may not have been
/// read.
pub(super) fn read_bytes_fallible<T: Task>(
    task: &mut T,
    addr: RemotePtr<Void>,
    buf: &mut [u8],
) -> Result<usize, ()> {
    if buf.len() == 0 {
        return Ok(0);
    }

    match task.vm().local_mapping(addr, buf.len()) {
        Some(found) => {
            buf.copy_from_slice(&found[0..buf.len()]);
            return Ok(buf.len());
        }
        None => (),
    }

    if !task.vm().mem_fd().is_open() {
        return Ok(task.read_bytes_ptrace(addr, buf));
    }

    let mut all_read = 0;
    while all_read < buf.len() {
        unsafe { Errno::clear() };
        let nread: isize = unsafe {
            pread64(
                task.vm().mem_fd().as_raw(),
                buf.get_mut(all_read..)
                    .unwrap()
                    .as_mut_ptr()
                    .cast::<c_void>(),
                // How much more left to read
                buf.len() - all_read,
                // Where you're reading from in the tracee
                // This is of type off_t which is a i32 in x86 and i64 on x64
                (addr.as_usize() + all_read) as isize as _,
            )
        };
        // We open the mem_fd just after being notified of
        // exec(), when the Task is created.  Trying to read from that
        // fd seems to return 0 with errno 0.  Reopening the mem fd
        // allows the pwrite to succeed.  It seems that the first mem
        // fd we open, very early in exec, refers to the address space
        // before the exec and the second mem fd refers to the address
        // space after exec.
        if 0 == nread && 0 == all_read && 0 == errno() {
            // If we couldn't open the mem fd, then report 0 bytes read
            if !task.open_mem_fd() {
                // @TODO is this a wise decision?
                // Hmmm.. given that errno is 0 it seems logical.
                return Ok(0);
            }
            // Try again
            continue;
        }
        if nread <= 0 {
            if all_read > 0 {
                // We did successfully read _some_ data, so return success and ignore
                // any error.
                unsafe { Errno::clear() };
                return Ok(all_read);
            }
            return Err(());
        }
        // We read some data. We should try again in case we get short reads.
        all_read += nread as usize;
    }

    Ok(all_read)
}

/// Forwarded method definition
///
/// If the data can't all be read, then if `maybe_ok` is None, asserts otherwise
/// sets the inner mutable bool to false.
pub(super) fn read_bytes_helper<T: Task>(
    task: &mut T,
    addr: RemotePtr<Void>,
    buf: &mut [u8],
    maybe_ok: Option<&mut bool>,
) {
    // pread64 etc can't handle addresses that appear to be negative ...
    // like [vsyscall].
    let result_nread = task.read_bytes_fallible(addr, buf);
    match result_nread {
        Ok(nread) if nread == buf.len() => (),
        _ => {
            let nread = result_nread.unwrap_or(0);
            match maybe_ok {
                Some(ok) => *ok = false,
                None => {
                    ed_assert!(
                        task,
                        false,
                        "Should have read {} bytes from {}, but only read {}",
                        buf.len(),
                        addr,
                        nread
                    );
                }
            }
        }
    }
}

/// NOT a Forwarded method due to extra template parameter
///
/// If the data can't all be read, then if `ok` is non-null, sets *ok to
/// false, otherwise asserts.
pub fn read_bytes_helper_for<T: Task, D>(
    task: &mut dyn Task,
    addr: RemotePtr<D>,
    data: &mut D,
    ok: Option<&mut bool>,
) {
    let buf = unsafe { std::slice::from_raw_parts_mut(data as *mut D as *mut u8, size_of::<D>()) };
    task.read_bytes_helper(RemotePtr::cast(addr), buf, ok);
}

/// Forwarded method definition
///
/// Read and return the C string located at `child_addr` in
/// this address space.
pub(super) fn read_c_str<T: Task>(task: &mut T, child_addr: RemotePtr<u8>) -> CString {
    // XXX handle invalid C strings
    // e.g. c-strings that don't end even when an unmapped region of memory
    // is reached.
    let mut p = child_addr;
    let mut s: Vec<u8> = Vec::new();
    loop {
        // We're only guaranteed that [child_addr, end_of_page) is mapped.
        // So be conservative and assume that c-string ends before the
        // end of the page. In case it _hasn't_ ended then we try on the
        // next page and so forth.
        let end_of_page: RemotePtr<Void> = ceil_page_size(p.as_usize() + 1).into();
        let nbytes: usize = end_of_page - p;
        let mut buf = Vec::<u8>::with_capacity(nbytes);
        buf.resize(nbytes, 0);
        task.read_bytes_helper(p, &mut buf, None);
        for i in 0..nbytes {
            if 0 == buf[i] {
                // We have already checked it so unsafe is OK!
                return unsafe { CString::from_vec_unchecked(s) };
            }
            s.push(buf[i]);
        }
        p = end_of_page;
    }
}

/// This is NOT a forwarded method
///
/// This function exists to work around
/// https://bugzilla.kernel.org/show_bug.cgi?id=99101.
/// On some kernels pwrite() to /proc/.../mem fails when writing to a region
/// that's PROT_NONE.
/// Also, writing through MAP_SHARED readonly mappings fails (even if the
/// file was opened read-write originally), so we handle that here too.
pub(super) fn safe_pwrite64(
    t: &mut dyn Task,
    buf: &[u8],
    addr: RemotePtr<Void>,
) -> Result<usize, ()> {
    let mut mappings_to_fix: Vec<(MemoryRangeKey, ProtFlags)> = Vec::new();
    let buf_size = buf.len();
    for (k, m) in &t.vm().maps_containing_or_after(floor_page_size(addr)) {
        if m.map.start() >= ceil_page_size(addr + buf_size) {
            break;
        }

        if m.map.prot().contains(ProtFlags::PROT_WRITE) {
            continue;
        }

        if !(m.map.prot().contains(ProtFlags::PROT_READ))
            || (m.map.flags().contains(MapFlags::MAP_SHARED))
        {
            mappings_to_fix.push((*k, m.map.prot()));
        }
    }

    if mappings_to_fix.is_empty() {
        return pwrite_all_fallible(t.vm().mem_fd().unwrap(), buf, addr.as_isize());
    }

    let mem_fd = t.vm().mem_fd().unwrap();
    let mprotect_syscallno: i32 = syscall_number_for_mprotect(t.arch());
    let mut remote = AutoRemoteSyscalls::new(t);
    for m in &mappings_to_fix {
        rd_infallible_syscall!(
            remote,
            mprotect_syscallno,
            m.0.start().as_usize(),
            m.0.size(),
            (m.1 | ProtFlags::PROT_WRITE).bits()
        );
    }

    let nwritten_result: Result<usize, ()> = pwrite_all_fallible(mem_fd, buf, addr.as_isize());

    for m in &mappings_to_fix {
        rd_infallible_syscall!(
            remote,
            mprotect_syscallno,
            m.0.start().as_usize(),
            m.0.size(),
            m.1.bits()
        );
    }

    nwritten_result
}

/// Forwarded method definition
///
/// `flags` is bits from WriteFlags.
pub(super) fn write_bytes_helper<T: Task>(
    task: &mut T,
    addr: RemotePtr<Void>,
    buf: &[u8],
    ok: Option<&mut bool>,
    flags: WriteFlags,
) {
    let buf_size = buf.len();
    if 0 == buf_size {
        return;
    }

    if let Some(local) = task.vm().local_mapping_mut(addr, buf_size) {
        local[0..buf.len()].copy_from_slice(buf);
        return;
    }

    if !task.vm().mem_fd().is_open() {
        let nwritten = task.write_bytes_ptrace(addr, buf);
        if nwritten > 0 {
            task.vm().notify_written(addr, nwritten, flags);
        }

        if ok.is_some() && nwritten < buf_size {
            *ok.unwrap() = false;
        }
        return;
    }

    unsafe {
        Errno::clear();
    }
    let nwritten_result = safe_pwrite64(task, buf, addr);
    // See comment in read_bytes_helper().
    if let Ok(0) = nwritten_result {
        task.open_mem_fd();
        // Try again
        return task.write_bytes_helper(addr, buf, ok, flags);
    }
    if errno() == EPERM {
        fatal!(
            "Can't write to /proc/{}/mem\n\
                        Maybe you need to disable grsecurity MPROTECT with:\n\
                           setfattr -n user.pax.flags -v 'emr' <executable>",
            task.tid
        );
    }

    let nwritten = nwritten_result.unwrap_or(0);
    if ok.is_some() {
        if nwritten < buf_size {
            *ok.unwrap() = false;
        }
    } else {
        ed_assert!(
            task,
            nwritten == buf_size,
            "Should have written {} bytes to {}, but only wrote {}",
            addr,
            buf_size,
            nwritten,
        );
    }
    if nwritten > 0 {
        task.vm().notify_written(addr, nwritten, flags);
    }
}

/// NOT Forwarded method definition
///
/// Read `val` from `child_addr`.
/// If the data can't all be read, then if `ok` is non-null
/// sets *ok to false, otherwise asserts.
pub fn read_val_mem<D>(task: &mut dyn Task, child_addr: RemotePtr<D>, ok: Option<&mut bool>) -> D {
    let mut v: D = unsafe { zeroed() };
    let u8_slice = unsafe { slice::from_raw_parts_mut(&raw mut v as *mut u8, size_of::<D>()) };
    task.read_bytes_helper(RemotePtr::cast(child_addr), u8_slice, ok);
    return v;
}

/// NOT Forwarded method definition
///
/// Read `count` values from `child_addr`.
pub fn read_mem<D: Clone>(
    task: &mut dyn Task,
    child_addr: RemotePtr<D>,
    count: usize,
    ok: Option<&mut bool>,
) -> Vec<D> {
    let mut v: Vec<D> = Vec::with_capacity(count);
    v.resize(count, unsafe { zeroed() });
    let u8_slice =
        unsafe { slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, count * size_of::<D>()) };
    task.read_bytes_helper(RemotePtr::cast(child_addr), u8_slice, ok);
    v
}

/// Forwarded method definition
///
pub(super) fn syscallbuf_data_size<T: Task>(task: &mut T) -> usize {
    let addr: RemotePtr<u32> = RemotePtr::cast(task.syscallbuf_child);
    read_val_mem::<u32>(task, addr + offset_of!(syscallbuf_hdr, num_rec_bytes), None) as usize
        + size_of::<syscallbuf_hdr>()
}

/// Forwarded method definition
///
/// Write `N` bytes from `buf` to `child_addr`, or don't return.
pub(super) fn write_bytes<T: Task>(task: &mut T, child_addr: RemotePtr<Void>, buf: &[u8]) {
    write_bytes_helper(task, child_addr, buf, None, WriteFlags::empty())
}

/// Forwarded method definition
///
pub(super) fn next_syscallbuf_record<T: Task>(task: &mut T) -> RemotePtr<syscallbuf_record> {
    // Next syscallbuf record is size_of the syscallbuf header + number of bytes in buffer
    let addr = RemotePtr::<u8>::cast(task.syscallbuf_child + 1usize);
    let num_rec_bytes_addr =
        RemotePtr::<u8>::cast(task.syscallbuf_child) + offset_of!(syscallbuf_hdr, num_rec_bytes);

    // @TODO: Here we have used our knowledge that `num_rec_bytes` is a u32.
    // There does not seem to be a generic way to get that information -- explore more later.
    let num_rec_bytes = read_val_mem(task, RemotePtr::<u32>::cast(num_rec_bytes_addr), None);
    RemotePtr::cast(addr + num_rec_bytes)
}

/// Forwarded method definition
///
pub(super) fn stored_record_size<T: Task>(
    task: &mut T,
    record: RemotePtr<syscallbuf_record>,
) -> u32 {
    let size_field_addr = RemotePtr::<u8>::cast(record) + offset_of!(syscallbuf_record, size);

    // @TODO: Here we have used our knowledge that `size` is a u32.
    // There does not seem to be a generic way to get that information -- explore more later.
    preload_interface::stored_record_size(read_val_mem(
        task,
        RemotePtr::<u32>::cast(size_field_addr),
        None,
    ))
}

/// NOT Forwarded method definition
///
/// Write single `val` to `child_addr`.
pub fn write_val_mem<D: 'static>(
    task: &mut dyn Task,
    child_addr: RemotePtr<D>,
    val: &D,
    ok: Option<&mut bool>,
) {
    write_val_mem_with_flags(task, child_addr, val, ok, WriteFlags::empty())
}

/// NOT Forwarded method definition
///
/// Write single `val` to `child_addr` and optionally specify a flag.
pub fn write_val_mem_with_flags<D: 'static>(
    task: &mut dyn Task,
    child_addr: RemotePtr<D>,
    val: &D,
    ok: Option<&mut bool>,
    flags: WriteFlags,
) {
    debug_assert!(type_has_no_holes::<D>());
    let data_slice = unsafe { slice::from_raw_parts(val as *const _ as *const u8, size_of::<D>()) };

    task.write_bytes_helper(RemotePtr::cast(child_addr), data_slice, ok, flags);
}

/// NOT Forwarded method definition
///
/// Write array of `val`s to `child_addr`.
pub fn write_mem<D: 'static>(
    task: &mut dyn Task,
    child_addr: RemotePtr<D>,
    val: &[D],
    ok: Option<&mut bool>,
) {
    debug_assert!(type_has_no_holes::<D>());
    let data_slice =
        unsafe { slice::from_raw_parts(val.as_ptr().cast::<u8>(), val.len() * size_of::<D>()) };
    task.write_bytes_helper(
        RemotePtr::cast(child_addr),
        data_slice,
        ok,
        WriteFlags::empty(),
    );
}

/// Forwarded method
///
/// Force the wait status of this to `status`, as if
/// `wait()/try_wait()` had returned it. Call this whenever a waitpid
/// returned activity for this past.
pub(super) fn did_waitpid<T: Task>(task: &mut T, mut status: WaitStatus) {
    // After PTRACE_INTERRUPT, any next two stops may be a group stop caused by
    // that PTRACE_INTERRUPT (or neither may be). This is because PTRACE_INTERRUPT
    // generally lets other stops win (and thus doesn't inject it's own stop), but
    // if the other stop was already done processing, even we didn't see it yet,
    // the stop will still be queued, so we could see the other stop and then the
    // PTRACE_INTERRUPT group stop.
    // When we issue PTRACE_INTERRUPT, we this set this counter to 2, and here
    // we decrement it on every stop such that while this counter is positive,
    // any group-stop could be one induced by PTRACE_INTERRUPT
    let mut siginfo_overriden = false;
    if task.expecting_ptrace_interrupt_stop > 0 {
        task.expecting_ptrace_interrupt_stop -= 1;
        if is_signal_triggered_by_ptrace_interrupt(status.maybe_group_stop_sig()) {
            // Assume this was PTRACE_INTERRUPT and thus treat this as
            // TIME_SLICE_SIGNAL instead.
            if task.session().is_recording() {
                // Force this timeslice to end
                task.session()
                    .as_record_mut()
                    .unwrap()
                    .scheduler_mut()
                    .expire_timeslice();
            }
            status = WaitStatus::for_stop_sig(TIME_SLICE_SIGNAL);
            task.pending_siginfo = Default::default();
            task.pending_siginfo.si_signo = TIME_SLICE_SIGNAL;
            task.pending_siginfo._sifields._sigpoll.si_fd = task.hpc.ticks_interrupt_fd();
            task.pending_siginfo.si_code = POLL_IN as i32;
            siginfo_overriden = true;
            task.expecting_ptrace_interrupt_stop = 0;
        }
    }

    if !siginfo_overriden && status.maybe_stop_sig().is_sig() {
        let mut local_pending_siginfo = Default::default();
        if !task.ptrace_if_alive(
            PTRACE_GETSIGINFO,
            RemotePtr::null(),
            PtraceData::WriteInto(u8_raw_slice_mut(&mut local_pending_siginfo)),
        ) {
            log!(LogDebug, "Unexpected process death for {}", task.tid);
            status = WaitStatus::for_ptrace_event(PTRACE_EVENT_EXIT);
        }
        task.pending_siginfo = local_pending_siginfo;
    }

    let original_syscallno = task.registers.original_syscallno();
    log!(LogDebug, "  (refreshing register cache)");
    // An unstable exit can cause a task to exit without us having run it, in
    // which case we might have pending register changes for it that are now
    // irrelevant. In that case we just throw away our register changes and use
    // whatever the kernel now has.
    if status.maybe_ptrace_event() != PTRACE_EVENT_EXIT {
        ed_assert!(
            task,
            !task.registers_dirty,
            "Registers shouldn't already be dirty"
        );
    }
    // If the task was not stopped, we don't need to read the registers.
    // In fact if we didn't start the thread, we may not have flushed dirty
    // registers but still received a PTRACE_EVENT_EXIT, in which case the
    // task's register values are not what they should be.
    if !task.is_stopped {
        let mut ptrace_regs: native_user_regs_struct = Default::default();
        if task.ptrace_if_alive(
            PTRACE_GETREGS,
            RemotePtr::null(),
            PtraceData::WriteInto(u8_raw_slice_mut(&mut ptrace_regs)),
        ) {
            task.registers.set_from_ptrace(&ptrace_regs);
            // @TODO rr does an if-defined here. However that may not be neccessary as there are
            // only 2 architectures that likely to be supported by this code-base in the future
            //
            // Check the architecture of the task by looking at the
            // cs segment register and checking if that segment is a long mode segment
            // (Linux always uses GDT entries for this, which are globally the same).
            let a: SupportedArch = if is_long_mode_segment(task.registers.cs() as u32) {
                SupportedArch::X64
            } else {
                SupportedArch::X86
            };
            if a != task.registers.arch() {
                task.registers = Registers::new(a);
                task.registers.set_from_ptrace(&ptrace_regs);
            }
        } else {
            log!(LogDebug, "Unexpected process death for {}", task.tid);
            status = WaitStatus::for_ptrace_event(PTRACE_EVENT_EXIT);
        }
    }

    task.is_stopped = true;
    task.wait_status = status;
    let more_ticks: Ticks = task.hpc.read_ticks(task);
    // We stop counting here because there may be things we want to do to the
    // tracee that would otherwise generate ticks.
    task.hpc.stop_counting();
    task.session().accumulate_ticks_processed(more_ticks);
    task.ticks += more_ticks;

    if status.maybe_ptrace_event() == PTRACE_EVENT_EXIT {
        task.seen_ptrace_exit_event = true;
    } else {
        if task.registers.singlestep_flag() {
            task.registers.clear_singlestep_flag();
            task.registers_dirty = true;
        }

        if task.last_resume_orig_cx != 0 {
            let new_cx: usize = task.registers.cx();
            // Un-fudge registers, if we fudged them to work around the KNL hardware quirk
            let cutoff: usize = single_step_coalesce_cutoff();
            ed_assert!(task, new_cx == cutoff - 1 || new_cx == cutoff);
            let local_last_resume_orig_cx = task.last_resume_orig_cx;
            task.registers
                .set_cx(local_last_resume_orig_cx - cutoff + new_cx);
            task.registers_dirty = true;
        }
        task.last_resume_orig_cx = 0;

        if task.did_set_breakpoint_after_cpuid {
            let bkpt_addr: RemoteCodePtr = task.address_of_last_execution_resume
                + trapped_instruction_len(task.singlestepping_instruction);
            if task.ip() == bkpt_addr.increment_by_bkpt_insn_length(task.arch()) {
                let mut r = task.regs_ref().clone();
                r.set_ip(bkpt_addr);
                task.set_regs(&r);
            }
            task.vm_shr_ptr()
                .remove_breakpoint(bkpt_addr, BreakpointType::BkptInternal, task);
            task.did_set_breakpoint_after_cpuid = false;
        }
        if (task.singlestepping_instruction == TrappedInstruction::Pushf
            || task.singlestepping_instruction == TrappedInstruction::Pushf16)
            && task.ip()
                == task.address_of_last_execution_resume
                    + trapped_instruction_len(task.singlestepping_instruction)
        {
            // We singlestepped through a pushf. Clear TF bit on stack.
            let sp: RemotePtr<u16> = RemotePtr::cast(task.regs_ref().sp());
            // If this address is invalid then we should have segfaulted instead of
            // retiring the instruction!
            let val: u16 = read_val_mem(task, sp, None);
            let write_val = val & !(X86_TF_FLAG as u16);
            write_val_mem(task, sp, &write_val, None);
        }
        task.singlestepping_instruction = TrappedInstruction::None;

        // We might have singlestepped at the resumption address and just exited
        // the kernel without executing the breakpoint at that address.
        // The kernel usually (always?) singlesteps an extra instruction when
        // we do this with PTRACE_SYSEMU_SINGLESTEP, but rd's ptrace emulation
        // doesn't and it's kind of a kernel bug.
        if task
            .vm()
            .get_breakpoint_type_at_addr(task.address_of_last_execution_resume)
            != BreakpointType::BkptNone
            && task.maybe_stop_sig() == SIGTRAP
            && !task.maybe_ptrace_event().is_ptrace_event()
            && task.ip()
                == task
                    .address_of_last_execution_resume
                    .increment_by_bkpt_insn_length(task.arch())
        {
            ed_assert!(task, more_ticks == 0);
            // When we resume execution and immediately hit a breakpoint, the original
            // syscall number can be reset to -1. Undo that, so that the register
            // state matches the state we'd be in if we hadn't resumed. ReplayTimeline
            // depends on resume-at-a-breakpoint being a noop.
            task.registers.set_original_syscallno(original_syscallno);
            task.registers_dirty = true;
        }

        // If we're in the rd page,  we may have just returned from an untraced
        // syscall there and while in the rd page registers need to be consistent
        // between record and replay. During replay most untraced syscalls are
        // replaced with "xor eax,eax" (right after a "movq -1, %rcx") so
        // rcx is always -1, but during recording it sometimes isn't after we've
        // done a real syscall.
        if task.is_in_rd_page() {
            let arch = task.arch();
            // N.B.: Cross architecture syscalls don't go through the rd page, so we
            // know what the architecture is.
            task.canonicalize_regs(arch);
        }
    }

    task.did_wait();
}

const AR_L: u32 = 1 << 21;

/// Helper method
fn is_long_mode_segment(segment: u32) -> bool {
    let ar: u32;
    unsafe { llvm_asm!("lar $1, $0" : "=r"(ar) : "r"(segment)) };
    ar & AR_L == AR_L
}

/// Helper method
///
/// The value of rcx above which the CPU doesn't properly handle singlestep for
/// string instructions. Right now, since only once CPU has this quirk, this
/// value is hardcoded, but could depend on the CPU architecture in the future.
fn single_step_coalesce_cutoff() -> usize {
    return 16;
}

/// Forwarded Method
///
/// Resume execution `how`, deliverying `sig` if nonzero.
/// After resuming, `wait_how`. In replay, reset hpcs and
/// request a tick period of tick_period. The default value
/// of tick_period is 0, which means effectively infinite.
/// If interrupt_after_elapsed is nonzero, we interrupt the task
/// after that many seconds have elapsed.
///
/// All tracee execution goes through here.
pub(super) fn resume_execution<T: Task>(
    task: &mut T,
    how: ResumeRequest,
    wait_how: WaitRequest,
    tick_period: TicksRequest,
    maybe_sig: Option<i32>,
) {
    task.will_resume_execution(how, wait_how, tick_period, maybe_sig);
    match tick_period {
        TicksRequest::ResumeNoTicks => (),
        TicksRequest::ResumeUnlimitedTicks => {
            task.hpc.reset(0);
            task.activate_preload_thread_locals(None);
        }
        TicksRequest::ResumeWithTicksRequest(tr) => {
            // DIFF NOTE: rr ensures that that ticks requested is at least 1 through a max
            // We assert for it.
            ed_assert!(task, tr >= 1 && tr <= MAX_TICKS_REQUEST);
            task.hpc.reset(tr);
            task.activate_preload_thread_locals(None);
        }
    }
    let sig_string = match maybe_sig {
        Some(sig) => format!(", signal: {}", signal_name(sig)),
        None => String::new(),
    };

    log!(
        LogDebug,
        "resuming execution of tid: {} with: {}{} tick_period: {:?}",
        task.tid,
        ptrace_req_name(how as u32),
        sig_string,
        tick_period
    );
    task.address_of_last_execution_resume = task.ip();
    task.how_last_execution_resumed = how;
    task.set_debug_status(0);

    if is_singlestep_resume(how) {
        work_around_knl_string_singlestep_bug(task);
        task.singlestepping_instruction = trapped_instruction_at(task, task.ip());
        if task.singlestepping_instruction == TrappedInstruction::CpuId {
            // In KVM virtual machines (and maybe others), singlestepping over CPUID
            // executes the following instruction as well. Work around that.
            let ip = task.ip();
            let len = trapped_instruction_len(task.singlestepping_instruction);
            let local_did_set_breakpoint_after_cpuid =
                task.vm_shr_ptr()
                    .add_breakpoint(task, ip + len, BreakpointType::BkptInternal);
            task.did_set_breakpoint_after_cpuid = local_did_set_breakpoint_after_cpuid;
        }
    }

    task.flush_regs();

    let mut wait_ret: pid_t = 0;
    if task.session().is_recording() {
        // There's a nasty race where a stopped task gets woken up by a SIGKILL
        // and advances to the PTRACE_EXIT_EVENT ptrace-stop just before we
        // send a PTRACE_CONT. Our PTRACE_CONT will cause it to continue and exit,
        // which means we don't get a chance to clean up robust futexes etc.
        // Avoid that by doing a waitpid() here to see if it has exited.
        // This doesn't fully close the race since in theory we could be preempted
        // between the waitpid and the ptrace_if_alive, giving another task
        // a chance to SIGKILL our tracee and advance it to the PTRACE_EXIT_EVENT,
        // or just letting the tracee be scheduled to process its pending SIGKILL.
        //
        let mut raw_status: i32 = 0;
        // tid is already stopped but like it was described above, the task may have gotten
        // woken up by a SIGKILL -- in that case we can try waiting on it with a WNOHANG.
        wait_ret = unsafe { waitpid(task.tid, &mut raw_status, WNOHANG | __WALL) };
        ed_assert!(
            task,
            0 <= wait_ret,
            "waitpid({}, NOHANG) failed with: {}",
            task.tid,
            wait_ret
        );
        let status = WaitStatus::new(raw_status);
        if wait_ret == task.tid {
            // In some (but not all) cases where the child was killed with SIGKILL,
            // we don't get PTRACE_EVENT_EXIT before it just exits.
            ed_assert!(
                task,
                status.maybe_ptrace_event() == PTRACE_EVENT_EXIT
                    || status.fatal_sig().unwrap_or(0) == SIGKILL,
                "got {:?}",
                status
            );
        } else {
            // 0 here means that no pids have changed state (WNOHANG)
            ed_assert!(
                task,
                0 == wait_ret,
                "waitpid({}, NOHANG) failed with: {}",
                task.tid,
                wait_ret
            );
        }
    }
    // @TODO DIFF NOTE: Its more accurate to check if `wait_ret == task.tid` instead of
    // saying wait_ret > 0 but we leave it be for now to be consistent with rr.
    if wait_ret > 0 {
        log!(LogDebug, "Task: {} exited unexpectedly", task.tid);
        // wait() will see this and report the ptrace-exit event.
        task.detected_unexpected_exit = true;
    } else {
        match maybe_sig {
            None => {
                task.ptrace_if_alive(how as u32, RemotePtr::null(), PtraceData::None);
            }
            Some(sig) => {
                task.ptrace_if_alive(
                    how as u32,
                    RemotePtr::null(),
                    PtraceData::ReadFrom(u8_raw_slice(&sig)),
                );
            }
        }
    }

    task.is_stopped = false;
    task.extra_registers = None;
    if WaitRequest::ResumeWait == wait_how {
        task.wait(None);
    }
}

fn work_around_knl_string_singlestep_bug<T: Task>(task: &mut T) {
    let cx: usize = task.regs_ref().cx();
    let cutoff: usize = single_step_coalesce_cutoff();
    // The extra cx >= cutoff check is just an optimization, to avoid the
    // moderately expensive load from ip() if we can
    if cpu_has_knl_string_singlestep_bug() && cx > cutoff && at_x86_string_instruction(task) {
        // KNL has a quirk where single-stepping a string instruction can step up
        // to 64 iterations. Work around this by fudging registers to force the
        // processor to execute one iteration and one interation only.
        log!(
            LogDebug,
            "Working around KNL single-step hardware bug (cx={})",
            cx
        );
        if cx > cutoff {
            task.last_resume_orig_cx = cx;
            let mut r = task.regs_ref().clone();
            // An arbitrary value < cutoff would work fine here, except 1, since
            // the last iteration of the loop behaves differently
            r.set_cx(cutoff);
            task.set_regs(&r);
        }
    }
}

lazy_static! {
    static ref CPU_HAS_KNL_STRING_SINGLESTEP_BUG_INIT: bool =
        cpu_has_knl_string_singlestep_bug_init();
}

fn cpu_has_knl_string_singlestep_bug_init() -> bool {
    (cpuid(CPUID_GETFEATURES, 0).eax & 0xF0FF0) == 0x50670
}

fn cpu_has_knl_string_singlestep_bug() -> bool {
    *CPU_HAS_KNL_STRING_SINGLESTEP_BUG_INIT
}

pub fn os_clone_into(_state: &CapturedState, _remote: &mut AutoRemoteSyscalls) -> TaskSharedPtr {
    unimplemented!()
}

fn on_syscall_exit_arch<Arch: Architecture>(t: &mut dyn Task, sys: i32, regs: &Registers) {
    t.session().accumulate_syscall_performed();

    if regs.original_syscallno() == SECCOMP_MAGIC_SKIP_ORIGINAL_SYSCALLNO {
        return;
    }

    // mprotect can change the protection status of some mapped regions before
    // failing.
    // SYS_rdcall_mprotect_record always fails with ENOSYS, though we want to
    // note its usage here.
    if regs.syscall_failed()
        && !is_mprotect_syscall(sys, regs.arch())
        && sys != Arch::RDCALL_MPROTECT_RECORD
    {
        return;
    }

    if sys == Arch::BRK || sys == Arch::MMAP || sys == Arch::MMAP2 || sys == Arch::MREMAP {
        log!(
            LogDebug,
            "(brk/mmap/mmap2/mremap will receive / has received direct processing)"
        );
        return;
    }

    if sys == Arch::RDCALL_MPROTECT_RECORD {
        unimplemented!()
    }

    if sys == Arch::MPROTECT {
        let addr: RemotePtr<Void> = regs.arg1().into();
        let num_bytes: usize = regs.arg2();
        let prot = regs.arg3_signed() as i32;
        let prot_flags = ProtFlags::from_bits(prot).unwrap();
        t.vm_shr_ptr().protect(t, addr, num_bytes, prot_flags);
    }

    if sys == Arch::MUNMAP {
        let addr: RemotePtr<Void> = regs.arg1().into();
        let num_bytes: usize = regs.arg2();
        return t.vm_shr_ptr().unmap(t, addr, num_bytes);
    }

    if sys == Arch::SHMDT {
        return process_shmdt(t, regs.arg1().into());
    }

    if sys == Arch::MADVISE {
        let addr: RemotePtr<Void> = regs.arg1().into();
        let num_bytes: usize = regs.arg2();
        let advice = regs.arg3() as i32;
        return t.vm_shr_ptr().advise(t, addr, num_bytes, advice);
    }

    if sys == Arch::IPC {
        match regs.arg1() as u32 {
            SHMDT => return process_shmdt(t, regs.arg5().into()),
            _ => return,
        }
    }

    if sys == Arch::SET_THREAD_AREA {
        t.set_thread_area(regs.arg1().into());
        return;
    }

    if sys == Arch::PRCTL {
        match t.regs_ref().arg1_signed() as i32 {
            PR_SET_SECCOMP => {
                if t.regs_ref().arg2() == SECCOMP_MODE_FILTER as usize && t.session().is_recording()
                {
                    t.seccomp_bpf_enabled = true;
                }
            }

            PR_SET_NAME => {
                t.update_prname(t.regs_ref().arg2().into());
            }

            _ => (),
        }
        return;
    }

    if sys == Arch::DUP || sys == Arch::DUP2 || sys == Arch::DUP3 {
        t.fd_table_shr_ptr().borrow_mut().did_dup(
            regs.arg1() as i32,
            regs.syscall_result() as i32,
            t,
        );
        return;
    }

    if sys == Arch::FCNTL64 || sys == Arch::FCNTL {
        if regs.arg2() == FcntlOperation::DUPFD as usize
            || regs.arg2() == FcntlOperation::DUPFD_CLOEXEC as usize
        {
            t.fd_table_shr_ptr().borrow_mut().did_dup(
                regs.arg1() as i32,
                regs.syscall_result() as i32,
                t,
            );
        }
        return;
    }

    if sys == Arch::CLOSE {
        t.fd_table_shr_ptr()
            .borrow_mut()
            .did_close(regs.arg1() as i32, t);
        return;
    }

    if sys == Arch::UNSHARE {
        if regs.arg1() & CLONE_FILES as usize != 0 {
            t.fd_table_mut().task_set_mut().erase(t.weak_self_ptr());
            t.fds = Some(t.fd_table_shr_ptr().borrow().clone_into_task(t));
        }
        return;
    }

    if sys == Arch::PWRITE64 || sys == Arch::WRITE {
        let fd: i32 = regs.arg1_signed() as i32;
        let mut ranges: Vec<file_monitor::Range> = Vec::new();
        let amount: isize = regs.syscall_result_signed();
        if amount > 0 {
            ranges.push(file_monitor::Range::new(
                regs.arg2().into(),
                amount as usize,
            ));
        }
        let mut offset = LazyOffset::new(t, &regs, sys);
        offset
            .task_mut()
            .fd_table_shr_ptr()
            .borrow_mut()
            .did_write(fd, ranges, &mut offset);
        return;
    }

    if sys == Arch::PWRITEV || sys == Arch::WRITEV {
        let fd: i32 = regs.arg1_signed() as i32;
        let mut ranges: Vec<file_monitor::Range> = Vec::new();
        let iovecs = read_mem(
            t,
            RemotePtr::<Arch::iovec>::new_from_val(regs.arg2()),
            regs.arg3(),
            None,
        );
        let mut written = regs.syscall_result_signed();
        ed_assert!(t, written >= 0);
        for v in iovecs {
            let (iov_remote_ptr, iov_len) = Arch::get_iovec(&v);
            let amount = min(written, iov_len.try_into().unwrap());
            if amount > 0 {
                ranges.push(file_monitor::Range::new(iov_remote_ptr, amount as usize));
                written -= amount;
            }
        }
        let mut offset = LazyOffset::new(t, &regs, sys);
        offset
            .task_mut()
            .fd_table_shr_ptr()
            .borrow_mut()
            .did_write(fd, ranges, &mut offset);
        return;
    }

    if sys == Arch::PTRACE {
        process_ptrace::<Arch>(regs, t);
        return;
    }
}

/// Forwarded method definition
///
/// Call this hook just before exiting a syscall.  Often Task
/// attributes need to be updated based on the finishing syscall.
/// Use 'regs' instead of this->regs() because some registers may not be
/// set properly in the task yet.
pub(super) fn on_syscall_exit(
    t: &mut dyn Task,
    syscallno: i32,
    arch: SupportedArch,
    regs: &Registers,
) {
    with_converted_registers(regs, arch, |regs| {
        rd_arch_function_selfless!(on_syscall_exit_arch, arch, t, syscallno, regs);
    })
}

/// Call this method when this task has exited a successful execve() syscall.
/// At this point it is safe to make remote syscalls.
pub(super) fn post_exec_syscall(t: &mut dyn Task) {
    let arch = t.arch();
    t.canonicalize_regs(arch);
    t.vm_shr_ptr().post_exec_syscall(t);

    if SessionInner::has_cpuid_faulting() {
        let mut remote = AutoRemoteSyscalls::new(t);
        rd_infallible_syscall!(
            remote,
            syscall_number_for_arch_prctl(arch),
            ARCH_SET_CPUID,
            0
        );
    }
}

/// Forwarded Method
///
/// DIFF NOTE: Simply called post_exec(...) in rr
/// Not to be confused with another post_exec() in rr that does not
/// take any arguments
pub(super) fn post_exec_for_exe<T: Task>(t: &mut T, exe_file: &OsStr) {
    let mut stopped_task_in_address_space = None;
    let mut other_task_in_address_space = false;
    for task in t.vm().task_set().iter_except(t.weak_self_ptr()) {
        other_task_in_address_space = true;
        if task.borrow().is_stopped {
            stopped_task_in_address_space = Some(task);
            break;
        }
    }
    match stopped_task_in_address_space {
        Some(stopped_task) => {
            let mut t = stopped_task.borrow_mut();
            let syscallbuf_child = t.syscallbuf_child;
            let syscallbuf_size = t.syscallbuf_size;
            let scratch_ptr = t.scratch_ptr;
            let scratch_size = t.scratch_size;
            let mut remote = AutoRemoteSyscalls::new(t.as_mut());
            unmap_buffers_for(
                &mut remote,
                syscallbuf_child,
                syscallbuf_size,
                scratch_ptr,
                scratch_size,
            );
        }
        None => {
            if other_task_in_address_space {
                // We should clean up our syscallbuf/scratch but that's too hard since we
                // have no stopped task to use for that :-(.
                // (We can't clean up those buffers *before* the exec completes, because it
                // might fail in which case we shouldn't have cleaned them up.)
                // Just let the buffers leak. The AddressSpace will clean up our local
                // shared buffer when it's destroyed.
                log!(
                    LogWarn,
                    "Intentionally leaking syscallbuf after exec for task {}",
                    t.tid
                );
            }
        }
    }
    t.session().post_exec(t);

    t.vm().task_set_mut().erase(t.weak_self_ptr());
    t.fd_table_mut().task_set_mut().erase(t.weak_self_ptr());

    t.extra_registers = None;
    let mut e = t.extra_regs_ref().clone();
    e.reset();
    t.set_extra_regs(&e);

    t.syscallbuf_child = RemotePtr::null();
    t.syscallbuf_size = 0;
    t.scratch_ptr = RemotePtr::null();
    t.cloned_file_data_fd_child = -1;
    t.stopping_breakpoint_table = RemoteCodePtr::null();
    t.stopping_breakpoint_table_entry_size = 0;
    t.preload_globals = None;
    t.thread_group_mut().execed = true;
    t.thread_areas_.clear();
    t.thread_locals = [0u8; PRELOAD_THREAD_LOCALS_SIZE];
    let exec_count = t.vm().uid().exec_count() + 1;
    t.as_ = Some(t.session().create_vm(t, Some(exe_file), Some(exec_count)));
    // It's barely-documented, but Linux unshares the fd table on exec
    t.fds = Some(t.fd_table_shr_ptr().borrow().clone_into_task(t));
    let prname = prname_from_exe_image(t.vm().exe_image());
    t.prname = prname.to_owned();
}

fn prname_from_exe_image(exe_image: &OsStr) -> &OsStr {
    let len = exe_image.as_bytes().len();
    debug_assert!(len > 0);
    let maybe_pos = exe_image.as_bytes().iter().rposition(|&c| c == b'/');
    let pos = match maybe_pos {
        Some(loc) if loc == len => {
            fatal!("empty prname?? {:?}", exe_image);
            unreachable!();
        }
        Some(loc) => loc + 1,
        None => 0,
    };
    OsStr::from_bytes(&exe_image.as_bytes()[pos..])
}

/// Forwarded method definition
///
/// Determine why a SIGTRAP occurred. Uses debug_status() but doesn't
/// consume it.
pub(super) fn compute_trap_reasons<T: Task>(t: &mut T) -> TrapReasons {
    ed_assert!(t, t.maybe_stop_sig() == SIGTRAP);
    let mut reasons = TrapReasons::default();
    let status = t.debug_status();
    reasons.singlestep = status & DebugStatus::DsSingleStep as usize != 0;

    let addr_last_execution_resume = t.address_of_last_execution_resume;
    if is_singlestep_resume(t.how_last_execution_resumed) {
        if is_at_syscall_instruction(t, addr_last_execution_resume)
            && t.ip() == addr_last_execution_resume + syscall_instruction_length(t.arch())
        {
            // During replay we execute syscall instructions in certain cases, e.g.
            // mprotect with syscallbuf. The kernel does not set DS_SINGLESTEP when we
            // step over those instructions so we need to detect that here.
            reasons.singlestep = true;
        } else {
            let ti: TrappedInstruction = trapped_instruction_at(t, addr_last_execution_resume);
            if ti == TrappedInstruction::CpuId
                && t.ip()
                    == addr_last_execution_resume
                        + trapped_instruction_len(TrappedInstruction::CpuId)
            {
                // Likewise we emulate CPUID instructions and must forcibly detect that
                // here.
                reasons.singlestep = true;
            // This also takes care of the did_set_breakpoint_after_cpuid workaround case
            } else if ti == TrappedInstruction::Int3
                && t.ip()
                    == addr_last_execution_resume
                        + trapped_instruction_len(TrappedInstruction::Int3)
            {
                // INT3 instructions should also be turned into a singlestep here.
                reasons.singlestep = true;
            }
        }
    }

    // In VMWare Player 6.0.4 build-2249910, 32-bit Ubuntu x86 guest,
    // single-stepping does not trigger watchpoints :-(. So we have to
    // check watchpoints here. fast_forward also hides watchpoint changes.
    // Write-watchpoints will detect that their value has changed and trigger.
    // XXX Read/exec watchpoints can't be detected this way so they're still
    // broken in the above configuration :-(.
    if status & (DebugStatus::DsWatchpointAny as usize | DebugStatus::DsSingleStep as usize) != 0 {
        t.vm().notify_watchpoint_fired(
            status,
            if is_singlestep_resume(t.how_last_execution_resumed) {
                addr_last_execution_resume
            } else {
                RemoteCodePtr::null()
            },
        );
    }
    reasons.watchpoint = t.vm().has_any_watchpoint_changes()
        || (status & DebugStatus::DsWatchpointAny as usize != 0);

    // If we triggered a breakpoint, this would be the address of the breakpoint
    let ip_at_breakpoint: RemoteCodePtr = t.ip().decrement_by_bkpt_insn_length(t.arch());
    // Don't trust siginfo to report execution of a breakpoint if singlestep or
    // watchpoint triggered.
    if reasons.singlestep {
        reasons.breakpoint = AddressSpace::is_breakpoint_instruction(t, addr_last_execution_resume);
        if reasons.breakpoint {
            ed_assert!(t, addr_last_execution_resume == ip_at_breakpoint);
        }
    } else if reasons.watchpoint {
        // We didn't singlestep, so watchpoint state is completely accurate.
        // The only way the last instruction could have triggered a watchpoint
        // and be a breakpoint instruction is if an EXEC watchpoint fired
        // at the breakpoint address.
        reasons.breakpoint = t.vm().has_exec_watchpoint_fired(ip_at_breakpoint)
            && AddressSpace::is_breakpoint_instruction(t, ip_at_breakpoint);
    } else {
        let si = *t.get_siginfo();
        ed_assert!(t, SIGTRAP == si.si_signo, " expected SIGTRAP, got {:?}", si);
        reasons.breakpoint = is_kernel_trap(si.si_code);

        let is_a_breakpoint = AddressSpace::is_breakpoint_instruction(t, ip_at_breakpoint);
        if reasons.breakpoint {
            ed_assert!(
                t,
                is_a_breakpoint,
                " expected breakpoint at {}, got siginfo {:?}",
                ip_at_breakpoint,
                si
            )
        }
    }
    reasons
}

pub(super) fn at_preload_init_common<T: Task>(t: &mut T) {
    t.vm_shr_ptr().at_preload_init(t);
    do_preload_init(t);

    t.fd_table_shr_ptr()
        .borrow()
        .init_syscallbuf_fds_disabled(t);
}

fn do_preload_init_arch<Arch: Architecture, T: Task>(t: &mut T) {
    let addr_val = t.regs_ref().arg1();
    let params = read_val_mem(
        t,
        RemotePtr::<Arch::rdcall_init_preload_params>::new_from_val(addr_val),
        None,
    );

    let res = Arch::rdcall_init_preload_params_globals(&params);
    t.preload_globals = Some(res.0);
    t.stopping_breakpoint_table = res.1;
    t.stopping_breakpoint_table_entry_size = res.2;
    for rc_t in t.vm().task_set().iter_except(t.weak_self_ptr()) {
        let mut tt = rc_t.borrow_mut();
        tt.preload_globals = Some(res.0);

        tt.stopping_breakpoint_table = res.1;
        tt.stopping_breakpoint_table_entry_size = res.2;
    }

    let preload_globals_ptr: RemotePtr<bool> = RemotePtr::cast(t.preload_globals.unwrap());
    let addr = preload_globals_ptr + offset_of!(preload_globals, in_replay);
    let is_replaying = t.session().is_replaying();
    write_val_mem(t, addr, &is_replaying, None);
}

fn do_preload_init<T: Task>(t: &mut T) {
    rd_arch_task_function_selfless!(T, do_preload_init_arch, t.arch(), t);
}

/// (Note: Methods following this are protected in the rr implementation)
/// Return a new Task cloned from `p`.  `flags` are a set of
/// CloneFlags (see above) that determine which resources are
/// shared or copied to the new child.  `new_tid` is the tid
/// assigned to the new task by the kernel.  `new_rec_tid` is
/// only relevant to replay, and is the pid that was assigned
/// to the task during recording.
///
/// NOTE: Called simply Task::clone() in rr.
///
/// @TODO Can make a template parameter for T:Task but will need to add a
/// method to the Task Trait. Since this is a protected method in rr may
/// want to think about this some more...
pub(in super::super) fn clone_task_common(
    clone_this: &mut dyn Task,
    reason: CloneReason,
    flags: CloneFlags,
    stack: RemotePtr<Void>,
    tls: RemotePtr<Void>,
    _cleartid_addr: RemotePtr<i32>,
    new_tid: pid_t,
    new_rec_tid: Option<pid_t>,
    new_serial: u32,
    maybe_other_session: Option<Rc<Box<dyn Session>>>,
) -> TaskSharedPtr {
    let mut new_task_session = clone_this.session();
    match maybe_other_session {
        Some(other_session) => {
            ed_assert!(clone_this, reason != CloneReason::TraceeClone);
            new_task_session = other_session;
        }
        None => {
            ed_assert!(clone_this, reason == CloneReason::TraceeClone);
        }
    }
    // No longer mutable.
    let new_task_session = new_task_session;

    let mut t: Box<dyn Task> =
        new_task_session.new_task(new_tid, new_rec_tid, new_serial, clone_this.arch());

    if flags.contains(CloneFlags::CLONE_SHARE_VM) {
        // The cloned task has the same AddressSpace
        t.as_ = clone_this.as_.clone();
        if !stack.is_null() {
            let last_stack_byte: RemotePtr<Void> = stack - 1usize;
            match t.vm_shr_ptr().mapping_of(last_stack_byte) {
                Some(mapping) => {
                    if !mapping.recorded_map.is_heap() {
                        let m: &KernelMapping = &mapping.map;
                        log!(LogDebug, "mapping stack for {} at {}", new_tid, m);
                        let m_start = m.start();
                        let m_size = m.size();
                        let m_prot = m.prot();
                        let m_flags = m.flags();
                        let m_file_offset_bytes = m.file_offset_bytes();
                        let m_device = m.device();
                        let m_inode = m.inode();

                        // Release the borrow because we may want to modify the vm MemoryMap
                        drop(mapping);
                        t.vm_shr_ptr().map(
                            &mut *t,
                            m_start,
                            m_size,
                            m_prot,
                            m_flags,
                            m_file_offset_bytes,
                            OsStr::new("[stack]"),
                            m_device,
                            m_inode,
                            None,
                            None,
                            None,
                            None,
                            None,
                        );
                    }
                }
                None => (),
            };
        }
    } else {
        t.as_ = Some(new_task_session.clone_vm(&mut *t, clone_this.vm_shr_ptr()));
    }

    t.syscallbuf_size = clone_this.syscallbuf_size;
    t.stopping_breakpoint_table = clone_this.stopping_breakpoint_table;
    t.stopping_breakpoint_table_entry_size = clone_this.stopping_breakpoint_table_entry_size;
    t.preload_globals = clone_this.preload_globals;
    t.seccomp_bpf_enabled = clone_this.seccomp_bpf_enabled;

    let rc_t: TaskSharedPtr = Rc::new(RefCell::new(t));
    let weak_self_ptr = Rc::downgrade(&rc_t);
    rc_t.borrow_mut().weak_self = weak_self_ptr.clone();

    let mut ref_t = rc_t.borrow_mut();
    // FdTable is either shared or copied, so the contents of
    // syscallbuf_fds_disabled_child are still valid.
    if flags.contains(CloneFlags::CLONE_SHARE_FILES) {
        ref_t.fds = clone_this.fds.clone();
        ref_t
            .fd_table_shr_ptr()
            .borrow_mut()
            .task_set_mut()
            .insert(weak_self_ptr.clone());
    } else {
        ref_t.fds = Some(clone_this.fd_table().clone_into_task(ref_t.as_mut()));
    }

    ref_t.top_of_stack = stack;
    // Clone children, both thread and fork, inherit the parent
    // prname.
    ref_t.prname = clone_this.prname.clone();

    // wait() before trying to do anything that might need to
    // use ptrace to access memory
    ref_t.wait(None);

    ref_t.post_wait_clone(clone_this, flags);
    if flags.contains(CloneFlags::CLONE_SHARE_THREAD_GROUP) {
        ref_t.tg = clone_this.tg.clone();
    } else {
        ref_t.tg =
            Some(new_task_session.clone_tg(ref_t.as_mut(), clone_this.thread_group_shr_ptr()));
    }
    ref_t
        .thread_group_shr_ptr()
        .borrow_mut()
        .task_set_mut()
        .insert(weak_self_ptr.clone());

    ref_t.open_mem_fd_if_needed();
    ref_t.thread_areas_ = clone_this.thread_areas_.clone();
    if flags.contains(CloneFlags::CLONE_SET_TLS) {
        set_thread_area_from_clone(ref_t.as_mut(), tls);
    }

    ref_t
        .vm_shr_ptr()
        .task_set_mut()
        .insert(weak_self_ptr.clone());

    if reason == CloneReason::TraceeClone {
        if !flags.contains(CloneFlags::CLONE_SHARE_VM) {
            // Unmap syscallbuf and scratch for tasks running the original address
            // space.
            let mut remote = AutoRemoteSyscalls::new(ref_t.as_mut());
            // Leak the scratch buffer for the task we cloned from. We need to do
            // this because we may be using part of it for the syscallbuf stack
            // and unmapping it now would cause a crash in the new task.
            for tt in clone_this
                .vm()
                .task_set()
                .iter_except(clone_this.weak_self_ptr())
            {
                unmap_buffers_for(
                    &mut remote,
                    tt.borrow().syscallbuf_child,
                    tt.borrow().syscallbuf_size,
                    tt.borrow().scratch_ptr,
                    tt.borrow().scratch_size,
                );
            }
            clone_this.vm().did_fork_into(remote.task_mut());
        }

        if flags.contains(CloneFlags::CLONE_SHARE_FILES) {
            // Clear our desched_fd_child so that we don't try to close it.
            // It should only be closed in `clone_this`.
            ref_t.desched_fd_child = -1;
            ref_t.cloned_file_data_fd_child = -1;
        } else {
            // Close syscallbuf fds for tasks using the original fd table.
            let mut remote = AutoRemoteSyscalls::new(ref_t.as_mut());
            close_buffers_for(
                &mut remote,
                clone_this.desched_fd_child,
                clone_this.cloned_file_data_fd_child,
            );
            for tt in clone_this
                .fd_table()
                .task_set()
                .iter_except(clone_this.weak_self_ptr())
            {
                close_buffers_for(
                    &mut remote,
                    tt.borrow().desched_fd_child,
                    tt.borrow().cloned_file_data_fd_child,
                )
            }
        }
    }

    ref_t.post_vm_clone(reason, flags, clone_this);

    drop(ref_t);
    rc_t
}

fn set_thread_area_from_clone(t: &mut dyn Task, tls: RemotePtr<u8>) {
    rd_arch_function_selfless!(set_thread_area_from_clone_arch, t.arch(), t, tls)
}

fn set_thread_area_from_clone_arch<Arch: Architecture>(t: &mut dyn Task, tls: RemotePtr<Void>) {
    if Arch::CLONE_TLS_TYPE == CloneTLSType::UserDescPointer {
        t.set_thread_area(RemotePtr::cast(tls));
    }
}

// DIFF NOTE: Param list different from rr version
fn unmap_buffers_for(
    remote: &mut AutoRemoteSyscalls,
    other_saved_syscallbuf_child: RemotePtr<syscallbuf_hdr>,
    other_syscallbuf_size: usize,
    other_scratch_ptr: RemotePtr<Void>,
    other_scratch_size: usize,
) {
    let arch = remote.task().arch();
    if !other_scratch_ptr.is_null() {
        rd_infallible_syscall!(
            remote,
            syscall_number_for_munmap(arch),
            other_scratch_ptr.as_usize(),
            other_scratch_size
        );
        remote
            .task()
            .vm_shr_ptr()
            .unmap(remote.task(), other_scratch_ptr, other_scratch_size);
    }
    if !other_saved_syscallbuf_child.is_null() {
        rd_infallible_syscall!(
            remote,
            syscall_number_for_munmap(arch),
            other_saved_syscallbuf_child.as_usize(),
            other_syscallbuf_size
        );
        remote.task().vm_shr_ptr().unmap(
            remote.task(),
            RemotePtr::cast(other_saved_syscallbuf_child),
            other_syscallbuf_size,
        );
    }
}

// DIFF NOTE: Param list different from rr version
pub fn close_buffers_for(
    remote: &mut AutoRemoteSyscalls,
    other_desched_fd_child: i32,
    other_cloned_file_data_fd_child: i32,
) {
    let arch = remote.task().arch();
    if other_desched_fd_child >= 0 {
        if remote.task().session().is_recording() {
            rd_infallible_syscall!(
                remote,
                syscall_number_for_close(arch),
                other_desched_fd_child
            );
        }
        remote
            .task_mut()
            .fd_table_shr_ptr()
            .borrow_mut()
            .did_close(other_desched_fd_child, remote.task_mut());
    }
    if other_cloned_file_data_fd_child >= 0 {
        rd_infallible_syscall!(
            remote,
            syscall_number_for_close(arch),
            other_cloned_file_data_fd_child
        );
        remote
            .task_mut()
            .fd_table_shr_ptr()
            .borrow_mut()
            .did_close(other_cloned_file_data_fd_child, remote.task_mut());
    }
}

pub(super) fn post_vm_clone_common<T: Task>(
    t: &mut T,
    reason: CloneReason,
    flags: CloneFlags,
    origin: &mut dyn Task,
) -> bool {
    let mut created_preload_thread_locals_mapping: bool = false;
    if !flags.contains(CloneFlags::CLONE_SHARE_VM) {
        created_preload_thread_locals_mapping = t.vm_shr_ptr().post_vm_clone(t);
    }

    if reason == CloneReason::TraceeClone {
        t.setup_preload_thread_locals_from_clone(origin);
    }

    created_preload_thread_locals_mapping
}

/// Forwarded method definition
///
pub(super) fn destroy_buffers<T: Task>(t: &mut T) {
    let saved_syscallbuf_child = t.syscallbuf_child;
    let mut remote = AutoRemoteSyscalls::new(t);
    // Clear syscallbuf_child now so nothing tries to use it while tearing
    // down buffers.
    remote.task_mut().syscallbuf_child = RemotePtr::null();
    let syscallbuf_size = remote.task().syscallbuf_size;
    let scratch_ptr = remote.task().scratch_ptr;
    let scratch_size = remote.task().scratch_size;
    unmap_buffers_for(
        &mut remote,
        saved_syscallbuf_child,
        syscallbuf_size,
        scratch_ptr,
        scratch_size,
    );
    remote.task_mut().scratch_ptr = RemotePtr::null();
    let other_desched_fd_child = remote.task().desched_fd_child;
    let other_cloned_file_data_fd_child = remote.task().cloned_file_data_fd_child;
    close_buffers_for(
        &mut remote,
        other_desched_fd_child,
        other_cloned_file_data_fd_child,
    );
    remote.task_mut().desched_fd_child = -1;
    remote.task_mut().cloned_file_data_fd_child = -1;
}

/// Takes sess as a param because sess might be in the process of
/// getting drop()-ed. Calling t.session() will not be a good idea
/// in this case because the weak point upgrade will fail as the
/// session is gettng dropped.
pub(super) fn task_drop_common<T: Task>(t: &T) {
    log!(LogDebug, "task {} (rec:{}) is dying ...", t.tid, t.rec_tid);

    t.fallible_ptrace(PTRACE_DETACH, RemotePtr::null(), PtraceData::None);

    if t.unstable.get() {
        log!(
            LogWarn,
            "{} is unstable; not blocking on its termination",
            t.tid
        );
        // This will probably leak a zombie process for rd's lifetime.

        // Destroying a Session may result in unstable exits during which
        // destroy_buffers() will not have been called.
        if !t.syscallbuf_child.is_null() {
            t.vm_shr_ptr()
                .unmap(t, RemotePtr::cast(t.syscallbuf_child), t.syscallbuf_size);
        }
    } else {
        ed_assert!(t, t.seen_ptrace_exit_event);
        ed_assert!(t, t.syscallbuf_child.is_null());

        if t.thread_group().task_set().is_empty() && !t.session().is_recording() {
            // Reap the zombie.
            let ret = unsafe { waitpid(t.thread_group().real_tgid, ptr::null_mut(), __WALL) };
            if ret == -1 {
                ed_assert!(t, errno() == ECHILD || errno() == ESRCH);
            } else {
                ed_assert!(t, ret == t.thread_group().real_tgid);
            }
        }
    }

    // Session Rc may be getting drop()-ed so we may not have access to it
    // via weak pointer
    t.try_session().map(|sess| sess.on_destroy_task(t.tuid()));
    t.thread_group_shr_ptr()
        .borrow_mut()
        .task_set_mut()
        .erase(t.weak_self_ptr());
    t.vm_shr_ptr().task_set_mut().erase(t.weak_self_ptr());
    t.fd_table_shr_ptr()
        .borrow_mut()
        .task_set_mut()
        .erase(t.weak_self_ptr());

    log!(LogDebug, "  dead");
}

/// Forwarded method definition
///
pub(super) fn set_thread_area<T: Task>(t: &mut T, tls: RemotePtr<user_desc>) {
    let desc: user_desc = read_val_mem(t, tls, None);
    set_thread_area_core(&mut t.thread_areas_, desc)
}

pub(super) fn set_thread_area_core(thread_areas: &mut Vec<user_desc>, desc: user_desc) {
    for t in thread_areas.iter_mut() {
        if t.entry_number == desc.entry_number {
            *t = desc;
            return;
        }
    }

    thread_areas.push(desc);
}

fn process_shmdt(t: &dyn Task, addr: RemotePtr<Void>) {
    let size: usize = t.vm().get_shm_size(addr);
    t.vm().remove_shm_size(addr);
    t.vm_shr_ptr().unmap(t, addr, size);
}

fn process_ptrace<Arch: Architecture>(regs: &Registers, t: &mut dyn Task) {
    let pid = regs.arg2_signed() as pid_t;
    let maybe_tracee = t.session().find_task_from_rec_tid(pid);
    match regs.arg1() as u32 {
        PTRACE_SETREGS => {
            let tracee_rc = maybe_tracee.unwrap();
            let mut tracee = tracee_rc.borrow_mut();
            let data = read_mem(
                t,
                RemotePtr::<u8>::from(regs.arg4()),
                size_of::<Arch::user_regs_struct>(),
                None,
            );
            let mut r: Registers = tracee.regs_ref().clone();
            r.set_from_ptrace_for_arch(Arch::arch(), &data);
            tracee.set_regs(&r);
            return;
        }
        PTRACE_SETFPREGS => {
            let data = read_mem(
                t,
                RemotePtr::<u8>::from(regs.arg4()),
                size_of::<Arch::user_fpregs_struct>(),
                None,
            );
            let mut r = t.extra_regs_ref().clone();
            r.set_user_fpregs_struct(t, Arch::arch(), &data);
            t.set_extra_regs(&r);
            return;
        }
        PTRACE_SETFPXREGS => {
            let data = read_val_mem(
                t,
                RemotePtr::<x86::user_fpxregs_struct>::from(regs.arg4()),
                None,
            );
            let mut r = t.extra_regs_ref().clone();
            r.set_user_fpxregs_struct(t, &data);
            t.set_extra_regs(&r);
            return;
        }
        PTRACE_SETREGSET => {
            match regs.arg3() as u32 {
                NT_PRSTATUS => {
                    let tracee_rc = maybe_tracee.unwrap();
                    let mut tracee = tracee_rc.borrow_mut();
                    let set =
                        ptrace_get_regs_set::<Arch>(t, regs, size_of::<Arch::user_regs_struct>());
                    let mut r = tracee.regs_ref().clone();
                    r.set_from_ptrace_for_arch(Arch::arch(), &set);
                    tracee.set_regs(&r);
                }
                NT_FPREGSET => {
                    let tracee_rc = maybe_tracee.unwrap();
                    let mut tracee = tracee_rc.borrow_mut();
                    let set =
                        ptrace_get_regs_set::<Arch>(t, regs, size_of::<Arch::user_fpregs_struct>());
                    let mut r: ExtraRegisters = tracee.extra_regs_ref().clone();
                    r.set_user_fpregs_struct(t, Arch::arch(), &set);
                    tracee.set_extra_regs(&r);
                }
                NT_X86_XSTATE => {
                    let tracee_rc = maybe_tracee.unwrap();
                    let mut tracee = tracee_rc.borrow_mut();
                    match tracee.extra_regs_ref().format() {
                        Format::XSave => {
                            let set = ptrace_get_regs_set::<Arch>(
                                t,
                                regs,
                                tracee.extra_regs_ref().data_size(),
                            );
                            let mut r = ExtraRegisters::default();
                            let layout: XSaveLayout;
                            let session = t.session();
                            let maybe_replay = session.as_replay();
                            match maybe_replay {
                                Some(replay) => {
                                    layout = xsave_layout_from_trace(
                                        replay.trace_reader().cpuid_records(),
                                    );
                                }
                                None => {
                                    layout = xsave_native_layout().clone();
                                }
                            };
                            let ok = r.set_to_raw_data(tracee.arch(), Format::XSave, &set, layout);
                            ed_assert!(t, ok, "Invalid XSAVE data");
                            tracee.set_extra_regs(&r);
                        }
                        _ => {
                            ed_assert!(
                                t,
                                false,
                                "Unknown ExtraRegisters format; \n\
                                         Should have been caught during \n\
                                         prepare_ptrace"
                            );
                        }
                    }
                }
                _ => {
                    ed_assert!(
                        t,
                        false,
                        "Unknown regset type; Should have been \n\
                                        caught during prepare_ptrace"
                    );
                }
            }
            return;
        }
        PTRACE_POKEUSER => {
            let tracee_rc = maybe_tracee.unwrap();
            let mut tracee = tracee_rc.borrow_mut();
            let addr: usize = regs.arg3();
            let data: Arch::unsigned_word = Arch::as_unsigned_word(regs.arg4());
            if addr < size_of::<Arch::user_regs_struct>() {
                let mut r: Registers = tracee.regs_ref().clone();
                r.write_register_by_user_offset(addr, regs.arg4());
                tracee.set_regs(&r);
            } else {
                let u_debugreg_offset: usize = match Arch::arch() {
                    // Unfortunately we can't do something like offset_of!(Arch::user, u_debugreg)
                    // as rustc complains. Revisit to see if we can make this more generic.
                    SupportedArch::X64 => offset_of!(x64::user, u_debugreg),
                    SupportedArch::X86 => offset_of!(x86::user, u_debugreg),
                };

                // Assumes that there would be no fields added after u_debugreg[7]
                if addr >= u_debugreg_offset && addr < size_of::<Arch::user>() {
                    let regno: usize = (addr - u_debugreg_offset) / size_of_val(&data);
                    tracee.set_debug_reg(regno, regs.arg4());
                }
            }
            return;
        }
        PTRACE_ARCH_PRCTL => {
            let code = regs.arg4() as u32;
            match code {
                ARCH_GET_FS | ARCH_GET_GS => (),
                ARCH_SET_FS | ARCH_SET_GS => {
                    let tracee_rc = maybe_tracee.unwrap();
                    let mut tracee = tracee_rc.borrow_mut();
                    let mut r: Registers = tracee.regs_ref().clone();
                    if regs.arg3() == 0 {
                        // Work around a kernel bug in pre-4.7 kernels, where setting
                        // the gs/fs base to 0 via PTRACE_REGSET did not work correctly.
                        tracee.ptrace_if_alive(
                            PTRACE_ARCH_PRCTL,
                            regs.arg3().into(),
                            PtraceData::ReadWord(regs.arg4()),
                        );
                    }
                    if code == ARCH_SET_FS {
                        r.set_fs_base(regs.arg3() as u64);
                    } else {
                        r.set_gs_base(regs.arg3() as u64);
                    }
                    tracee.set_regs(&r);
                }
                _ => {
                    let tracee_rc = maybe_tracee.unwrap();
                    let tracee = tracee_rc.borrow_mut();
                    ed_assert!(tracee.as_ref(), false, "Should have detected this earlier");
                }
            };
            return;
        }
        _ => (),
    }
}

fn ptrace_get_regs_set<Arch: Architecture>(
    t: &mut dyn Task,
    regs: &Registers,
    min_size: usize,
) -> Vec<u8> {
    let iov = read_val_mem(t, RemotePtr::<Arch::iovec>::from(regs.arg4()), None);
    let (remote_ptr, iov_len) = Arch::get_iovec(&iov);
    ed_assert!(
        t,
        iov_len >= min_size,
        "Should have been caught during prepare_ptrace"
    );
    read_mem(t, remote_ptr, iov_len, None)
}
