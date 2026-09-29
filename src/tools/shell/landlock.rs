//! The sandbox for shell commands on Linux: Landlock confines what a
//! command can read and write, and a seccomp filter takes away its
//! sockets. See "The shell tool" in docs/ARCHITECTURE.md.
//!
//! Both are built once, in Jakkals's own process, and applied in the
//! child between `fork` and `exec` with a few syscalls that allocate
//! nothing, as code there must.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

// Landlock's ABI, from the kernel's include/uapi/linux/landlock.h.
// Kept here rather than taken from `libc` so the struct sizes are the
// ones this code was written against, whatever `libc` version.

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

const ACCESS_EXECUTE: u64 = 1 << 0;
const ACCESS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_READ_FILE: u64 = 1 << 2;
const ACCESS_READ_DIR: u64 = 1 << 3;
/// ABI 1's rights past these are about changing directories: removing
/// and making entries of every kind.
const ACCESS_ABI_1: u64 = (1 << 13) - 1;
/// ABI 2: linking or renaming a file into another directory.
const ACCESS_REFER: u64 = 1 << 13;
/// ABI 3: truncating a file, which before ABI 3 no rule could stop.
const ACCESS_TRUNCATE: u64 = 1 << 14;

/// The rights a rule on a file (not a directory) may carry.
const ACCESS_FILE: u64 = ACCESS_EXECUTE | ACCESS_WRITE_FILE | ACCESS_READ_FILE | ACCESS_TRUNCATE;

/// Every right Jakkals handles: all of ABI 3's. A right handled and not
/// granted by a rule is denied everywhere.
const ACCESS_HANDLED: u64 = ACCESS_ABI_1 | ACCESS_REFER | ACCESS_TRUNCATE;

/// What a command may do with the paths it can read.
const ACCESS_READ: u64 = ACCESS_EXECUTE | ACCESS_READ_FILE | ACCESS_READ_DIR;

/// The oldest Landlock ABI that can stop every write: ABI 3, Linux 6.2.
pub const ABI_REQUIRED: i64 = 3;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

// seccomp, from include/uapi/linux/{seccomp,filter,bpf_common,audit}.h.

/// BPF_LD | BPF_W | BPF_ABS: load a word of the call's data.
const BPF_LD_W_ABS: u16 = 0x20;
/// BPF_JMP | BPF_JEQ | BPF_K: jump if equal to a constant.
const BPF_JMP_JEQ_K: u16 = 0x15;
/// BPF_JMP | BPF_JGE | BPF_K: jump if at least a constant.
#[cfg(target_arch = "x86_64")]
const BPF_JMP_JGE_K: u16 = 0x35;
/// BPF_RET | BPF_K: return a constant verdict.
const BPF_RET_K: u16 = 0x06;
/// Offsets into `struct seccomp_data`.
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;
/// x86_64's x32 calls carry this bit: the same kernel, other numbers.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// What a program needs to start: the loader, its libraries and cache,
/// the time zones. `/usr` is walked instead, to leave out `/usr/local`,
/// where package managers install: never a system path.
const SYSTEM_READ: [&str; 11] = [
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/localtime",
    "/usr",
];

/// Devices a program may read. `/dev/null` may be written too.
const DEVICES: [&str; 4] = ["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"];

/// Why the Linux sandbox can't be set up.
#[derive(Debug)]
pub enum LandlockError {
    /// The kernel has no Landlock, or too old an ABI (0 for none).
    Abi(i64),
    /// A path to allow that can't be opened.
    Path { path: PathBuf, error: io::Error },
    /// A syscall that should work didn't.
    Syscall {
        call: &'static str,
        error: io::Error,
    },
}

/// The sandbox, built: a Landlock ruleset and a seccomp filter, ready to
/// be applied to each command.
pub struct Landlock {
    ruleset: OwnedFd,
    filter: Arc<[libc::sock_filter]>,
}

impl Landlock {
    /// Allows reading `root`, `read` and the system's paths. Without a
    /// `root`, it only shows that the sandbox can be built.
    pub fn new(root: Option<&Path>, read: &[PathBuf]) -> Result<Self, LandlockError> {
        let abi = abi();
        if abi < ABI_REQUIRED {
            return Err(LandlockError::Abi(abi.max(0)));
        }
        let attr = RulesetAttr {
            handled_access_fs: ACCESS_HANDLED,
        };
        // SAFETY: the kernel reads `size_of::<RulesetAttr>()` bytes from a
        // live struct and returns a new descriptor or -1.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(syscall_error("landlock_create_ruleset"));
        }
        let fd = RawFd::try_from(fd).expect("a descriptor fits an int");
        // SAFETY: `fd` was just returned to us and nothing else owns it.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };

        for path in root.into_iter().chain(read.iter().map(PathBuf::as_path)) {
            allow(&ruleset, path, ACCESS_READ)?;
        }
        for path in SYSTEM_READ {
            if path == "/usr" {
                allow_usr(&ruleset)?;
            } else {
                allow_if_there(&ruleset, Path::new(path), ACCESS_READ)?;
            }
        }
        for device in DEVICES {
            let access = if device == "/dev/null" {
                ACCESS_READ_FILE | ACCESS_WRITE_FILE
            } else {
                ACCESS_READ_FILE
            };
            allow_if_there(&ruleset, Path::new(device), access)?;
        }
        Ok(Self {
            ruleset,
            filter: filter().into(),
        })
    }

    /// Makes `command` confine itself before it runs its program.
    pub fn confine(&self, command: &mut Command) {
        let ruleset = self.ruleset.as_raw_fd();
        let filter = Arc::clone(&self.filter);
        let program = move || libc::sock_fprog {
            len: u16::try_from(filter.len()).expect("a short filter"),
            filter: filter.as_ptr().cast_mut(),
        };
        // SAFETY: runs in the child between fork and exec, where only
        // async-signal-safe calls belong: `prctl` and `syscall`, on a
        // descriptor and a filter built before the fork. Nothing here
        // allocates or takes a lock.
        unsafe {
            command.pre_exec(move || {
                // Required by both, and keeps a setuid program from
                // gaining what the sandbox takes away.
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let program = program();
                if libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &program as *const libc::sock_fprog,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

/// The kernel's Landlock ABI, or a negative number without Landlock.
pub fn abi() -> i64 {
    // SAFETY: with the version flag the kernel reads no memory and
    // returns the ABI or -1.
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    }
}

fn syscall_error(call: &'static str) -> LandlockError {
    LandlockError::Syscall {
        call,
        error: io::Error::last_os_error(),
    }
}

/// Grants `access` beneath `path`, or on it if it is a file.
fn allow(ruleset: &OwnedFd, path: &Path, access: u64) -> Result<(), LandlockError> {
    let path_error = |error| LandlockError::Path {
        path: path.to_owned(),
        error,
    };
    // O_PATH: a handle naming the file, which Landlock takes; nothing
    // is read through it.
    let file: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)
        .map_err(path_error)?;
    let is_dir = file.metadata().map_err(path_error)?.is_dir();
    let rule = PathBeneathAttr {
        allowed_access: if is_dir { access } else { access & ACCESS_FILE },
        parent_fd: file.as_raw_fd(),
    };
    // SAFETY: the kernel reads the packed rule from a live struct.
    let added = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &rule as *const PathBeneathAttr,
            0u32,
        )
    };
    if added != 0 {
        return Err(syscall_error("landlock_add_rule"));
    }
    Ok(())
}

/// [`allow`], for a system path this machine may not have.
fn allow_if_there(ruleset: &OwnedFd, path: &Path, access: u64) -> Result<(), LandlockError> {
    match allow(ruleset, path, access) {
        Err(LandlockError::Path { error, .. }) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Grants reading each entry of `/usr` but `local`.
fn allow_usr(ruleset: &OwnedFd) -> Result<(), LandlockError> {
    let entries = std::fs::read_dir("/usr").map_err(|error| LandlockError::Path {
        path: "/usr".into(),
        error,
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| LandlockError::Path {
            path: "/usr".into(),
            error,
        })?;
        if entry.file_name() != "local" {
            allow_if_there(ruleset, &entry.path(), ACCESS_READ)?;
        }
    }
    Ok(())
}

/// The seccomp filter: no sockets of any kind (so no network, and no
/// daemon reached through a Unix socket), and no io_uring, which could
/// open one without the `socket` call. Everything else is allowed;
/// calls from another architecture's table are refused whole.
fn filter() -> Vec<libc::sock_filter> {
    let statement = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let refused = libc::SECCOMP_RET_ERRNO | u32::try_from(libc::EPERM).expect("a small errno");
    let denied_calls = [libc::SYS_socket, libc::SYS_io_uring_setup]
        .map(|call| u32::try_from(call).expect("a syscall number fits u32"));

    // Jumps are counted forward from the next instruction; each check
    // below jumps to the refusal, the program's last instruction.
    let mut checks: Vec<(u16, u32)> = Vec::new();
    #[cfg(target_arch = "x86_64")]
    checks.push((BPF_JMP_JGE_K, X32_SYSCALL_BIT));
    for call in denied_calls {
        checks.push((BPF_JMP_JEQ_K, call));
    }
    let mut program = vec![
        statement(BPF_LD_W_ABS, DATA_ARCH),
        // Not this architecture: skip to the refusal, past the load, the
        // checks and the allow.
        libc::sock_filter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: u8::try_from(checks.len() + 2).expect("a short filter"),
            k: AUDIT_ARCH,
        },
        statement(BPF_LD_W_ABS, DATA_NR),
    ];
    let count = checks.len();
    for (index, (code, k)) in checks.into_iter().enumerate() {
        program.push(libc::sock_filter {
            code,
            // Past the checks left and the allow.
            jt: u8::try_from(count - index).expect("a short filter"),
            jf: 0,
            k,
        });
    }
    program.push(statement(BPF_RET_K, libc::SECCOMP_RET_ALLOW));
    program.push(statement(BPF_RET_K, refused));
    program
}
