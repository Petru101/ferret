// The Windows API calls the helper makes, declared by hand: the helper stays free of
// dependencies (std only) on Windows as on Linux (std + libc). Layouts are x64's.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type HANDLE = *mut c_void;
pub type BOOL = i32;

pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

pub const PROCESS_VM_READ: u32 = 0x0010;
pub const PROCESS_VM_WRITE: u32 = 0x0020;
pub const PROCESS_VM_OPERATION: u32 = 0x0008;
pub const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

pub const THREAD_GET_CONTEXT: u32 = 0x0008;
pub const THREAD_SET_CONTEXT: u32 = 0x0010;
pub const THREAD_SUSPEND_RESUME: u32 = 0x0002;
pub const THREAD_QUERY_INFORMATION: u32 = 0x0040;

pub const TH32CS_SNAPPROCESS: u32 = 0x2;
pub const TH32CS_SNAPTHREAD: u32 = 0x4;

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_IMAGE: u32 = 0x100_0000;
pub const MEM_MAPPED: u32 = 0x4_0000;
pub const PAGE_NOACCESS: u32 = 0x01;
pub const PAGE_READONLY: u32 = 0x02;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_WRITECOPY: u32 = 0x08;
pub const PAGE_EXECUTE: u32 = 0x10;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
pub const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
pub const PAGE_GUARD: u32 = 0x100;

pub const EXCEPTION_DEBUG_EVENT: u32 = 1;
pub const CREATE_PROCESS_DEBUG_EVENT: u32 = 3;
pub const EXIT_THREAD_DEBUG_EVENT: u32 = 4;
pub const EXIT_PROCESS_DEBUG_EVENT: u32 = 5;
pub const LOAD_DLL_DEBUG_EVENT: u32 = 6;
pub const DBG_CONTINUE: u32 = 0x0001_0002;
pub const DBG_EXCEPTION_NOT_HANDLED: u32 = 0x8001_0001;
pub const EXCEPTION_SINGLE_STEP: u32 = 0x8000_0004;
pub const EXCEPTION_BREAKPOINT: u32 = 0x8000_0003;
/// The single step and breakpoint of 32-bit code in a WOW64 process, as a 64-bit debugger
/// sees them.
pub const STATUS_WX86_SINGLE_STEP: u32 = 0x4000_001E;
pub const STATUS_WX86_BREAKPOINT: u32 = 0x4000_001F;

pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;

#[repr(C)]
pub struct MEMORY_BASIC_INFORMATION {
    pub BaseAddress: usize,
    pub AllocationBase: usize,
    pub AllocationProtect: u32,
    pub PartitionId: u16,
    pub RegionSize: usize,
    pub State: u32,
    pub Protect: u32,
    pub Type: u32,
}

#[repr(C)]
pub struct PROCESSENTRY32W {
    pub dwSize: u32,
    pub cntUsage: u32,
    pub th32ProcessID: u32,
    pub th32DefaultHeapID: usize,
    pub th32ModuleID: u32,
    pub cntThreads: u32,
    pub th32ParentProcessID: u32,
    pub pcPriClassBase: i32,
    pub dwFlags: u32,
    pub szExeFile: [u16; 260],
}

#[repr(C)]
pub struct THREADENTRY32 {
    pub dwSize: u32,
    pub cntUsage: u32,
    pub th32ThreadID: u32,
    pub th32OwnerProcessID: u32,
    pub tpBasePri: i32,
    pub tpDeltaPri: i32,
    pub dwFlags: u32,
}

/// DEBUG_EVENT: the union after the ids is kept as raw words (`u`); EXCEPTION_DEBUG_INFO's
/// code is its first 4 bytes, the faulting address at byte 16, first-chance at byte 152;
/// CREATE_PROCESS_DEBUG_INFO's and LOAD_DLL_DEBUG_INFO's file handle is the first word.
#[repr(C)]
pub struct DEBUG_EVENT {
    pub dwDebugEventCode: u32,
    pub dwProcessId: u32,
    pub dwThreadId: u32,
    pub u: [u64; 20],
}

impl DEBUG_EVENT {
    pub fn exception_code(&self) -> u32 {
        self.u[0] as u32
    }

    pub fn file_handle(&self) -> HANDLE {
        self.u[0] as usize as HANDLE
    }
}

#[repr(C)]
pub struct PROCESS_BASIC_INFORMATION {
    pub ExitStatus: isize,
    pub PebBaseAddress: usize,
    pub AffinityMask: usize,
    pub BasePriority: isize,
    pub UniqueProcessId: usize,
    pub InheritedFromUniqueProcessId: usize,
}

/// x64 CONTEXT (1232 bytes, 16-aligned), kept as bytes and read by offset.
#[repr(C, align(16))]
pub struct CONTEXT(pub [u8; 1232]);

pub const CONTEXT_AMD64: u32 = 0x0010_0000;
pub const CONTEXT_CONTROL: u32 = CONTEXT_AMD64 | 0x1;
pub const CONTEXT_INTEGER: u32 = CONTEXT_AMD64 | 0x2;
pub const CONTEXT_DEBUG_REGISTERS: u32 = CONTEXT_AMD64 | 0x10;

/// WOW64_CONTEXT (716 bytes), a 32-bit thread's registers.
#[repr(C, align(4))]
pub struct WOW64_CONTEXT(pub [u8; 716]);

pub const WOW64_CONTEXT_I386: u32 = 0x0001_0000;
pub const WOW64_CONTEXT_CONTROL: u32 = WOW64_CONTEXT_I386 | 0x1;
pub const WOW64_CONTEXT_INTEGER: u32 = WOW64_CONTEXT_I386 | 0x2;

macro_rules! field {
    ($ty:ty, $get:ident, $set:ident, $width:ty) => {
        impl $ty {
            pub fn $get(&self, off: usize) -> $width {
                <$width>::from_le_bytes(self.0[off..off + std::mem::size_of::<$width>()].try_into().unwrap())
            }

            pub fn $set(&mut self, off: usize, v: $width) {
                self.0[off..off + std::mem::size_of::<$width>()].copy_from_slice(&v.to_le_bytes());
            }
        }
    };
}
field!(CONTEXT, u64_at, set_u64, u64);
field!(CONTEXT, u32_at, set_u32, u32);
field!(WOW64_CONTEXT, u32_at, set_u32, u32);

impl CONTEXT {
    pub fn new(flags: u32) -> Box<CONTEXT> {
        let mut c = Box::new(CONTEXT([0; 1232]));
        c.set_u32(0x30, flags);
        c
    }
}

impl WOW64_CONTEXT {
    pub fn new(flags: u32) -> Box<WOW64_CONTEXT> {
        let mut c = Box::new(WOW64_CONTEXT([0; 716]));
        c.set_u32(0, flags);
        c
    }
}

/// Offsets in CONTEXT: debug registers 0-3, 6 and 7; rax..r15 in x86 encoding order; rip.
pub const CTX_DR: [usize; 4] = [0x48, 0x50, 0x58, 0x60];
pub const CTX_DR6: usize = 0x68;
pub const CTX_DR7: usize = 0x70;
pub const CTX_GPR: [usize; 16] = [0x78, 0x80, 0x88, 0x90, 0x98, 0xA0, 0xA8, 0xB0, 0xB8, 0xC0, 0xC8, 0xD0, 0xD8, 0xE0, 0xE8, 0xF0];
pub const CTX_RIP: usize = 0xF8;
pub const CTX_EFLAGS: usize = 0x44;
/// Offsets in WOW64_CONTEXT: eax..edi in x86 encoding order, eip, eflags.
pub const WOW_GPR: [usize; 8] = [0xB0, 0xAC, 0xA8, 0xA4, 0xC4, 0xB4, 0xA0, 0x9C];
pub const WOW_EIP: usize = 0xB8;
pub const WOW_EFLAGS: usize = 0xC0;

#[link(name = "kernel32")]
extern "system" {
    pub fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn ReadProcessMemory(p: HANDLE, addr: *const c_void, buf: *mut c_void, size: usize, read: *mut usize) -> BOOL;
    pub fn WriteProcessMemory(p: HANDLE, addr: *mut c_void, buf: *const c_void, size: usize, written: *mut usize) -> BOOL;
    pub fn VirtualQueryEx(p: HANDLE, addr: *const c_void, info: *mut MEMORY_BASIC_INFORMATION, len: usize) -> usize;
    pub fn K32GetMappedFileNameW(p: HANDLE, addr: *const c_void, name: *mut u16, size: u32) -> u32;
    pub fn QueryDosDeviceW(device: *const u16, target: *mut u16, max: u32) -> u32;
    pub fn QueryFullProcessImageNameW(p: HANDLE, flags: u32, name: *mut u16, size: *mut u32) -> BOOL;
    pub fn IsWow64Process(p: HANDLE, wow64: *mut BOOL) -> BOOL;
    pub fn GetCurrentProcessId() -> u32;
    pub fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> BOOL;
    pub fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    pub fn Process32FirstW(snap: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
    pub fn Process32NextW(snap: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
    pub fn Thread32First(snap: HANDLE, entry: *mut THREADENTRY32) -> BOOL;
    pub fn Thread32Next(snap: HANDLE, entry: *mut THREADENTRY32) -> BOOL;
    pub fn OpenThread(access: u32, inherit: BOOL, tid: u32) -> HANDLE;
    pub fn SuspendThread(t: HANDLE) -> u32;
    pub fn ResumeThread(t: HANDLE) -> u32;
    pub fn GetThreadContext(t: HANDLE, ctx: *mut CONTEXT) -> BOOL;
    pub fn SetThreadContext(t: HANDLE, ctx: *const CONTEXT) -> BOOL;
    pub fn Wow64GetThreadContext(t: HANDLE, ctx: *mut WOW64_CONTEXT) -> BOOL;
    pub fn Wow64SetThreadContext(t: HANDLE, ctx: *const WOW64_CONTEXT) -> BOOL;
    pub fn DebugActiveProcess(pid: u32) -> BOOL;
    pub fn DebugActiveProcessStop(pid: u32) -> BOOL;
    pub fn DebugSetProcessKillOnExit(kill: BOOL) -> BOOL;
    pub fn WaitForDebugEvent(event: *mut DEBUG_EVENT, ms: u32) -> BOOL;
    pub fn ContinueDebugEvent(pid: u32, tid: u32, status: u32) -> BOOL;
    pub fn GetStdHandle(which: u32) -> HANDLE;
    pub fn PeekNamedPipe(pipe: HANDLE, buf: *mut c_void, size: u32, read: *mut u32, avail: *mut u32, left: *mut u32) -> BOOL;
}

pub type HWND = *mut c_void;

#[link(name = "user32")]
extern "system" {
    pub fn EnumWindows(each: unsafe extern "system" fn(HWND, isize) -> BOOL, data: isize) -> BOOL;
    pub fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    pub fn IsWindowVisible(hwnd: HWND) -> BOOL;
}

pub const SC_MANAGER_ENUMERATE_SERVICE: u32 = 0x0004;
/// SERVICE_KERNEL_DRIVER | SERVICE_FILE_SYSTEM_DRIVER | SERVICE_WIN32_OWN_PROCESS |
/// SERVICE_WIN32_SHARE_PROCESS.
pub const SERVICE_DRIVERS_AND_WIN32: u32 = 0x0B | 0x30;
pub const SERVICE_ACTIVE: u32 = 0x1;
pub const ERROR_MORE_DATA: i32 = 234;

/// ENUM_SERVICE_STATUS_PROCESSW: the names (pointers into the same buffer) and nine DWORDs of
/// SERVICE_STATUS_PROCESS.
#[repr(C)]
pub struct ENUM_SERVICE_STATUS_PROCESSW {
    pub lpServiceName: *const u16,
    pub lpDisplayName: *const u16,
    pub status: [u32; 9],
}

#[link(name = "advapi32")]
extern "system" {
    pub fn OpenSCManagerW(machine: *const u16, database: *const u16, access: u32) -> HANDLE;
    pub fn CloseServiceHandle(h: HANDLE) -> BOOL;
    pub fn EnumServicesStatusExW(
        manager: HANDLE,
        level: u32,
        kind: u32,
        state: u32,
        services: *mut u8,
        size: u32,
        needed: *mut u32,
        returned: *mut u32,
        resume: *mut u32,
        group: *const u16,
    ) -> BOOL;
}

#[link(name = "ntdll")]
extern "system" {
    pub fn NtQueryInformationProcess(p: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

/// A handle closed when dropped.
pub struct Handle(pub HANDLE);

impl Handle {
    pub fn valid(h: HANDLE) -> Option<Handle> {
        (!h.is_null() && h != INVALID_HANDLE_VALUE).then_some(Handle(h))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

// Process handles are safe to use from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// A NUL-ended UTF-16 buffer to a String.
pub fn from_wide(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
