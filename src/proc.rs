//! Vivacité des process omp : nom du process (pid réutilisé) et abonnement à sa sortie, sans sondage.
//!
//! macOS : libproc et kqueue `EVFILT_PROC`/`NOTE_EXIT`. Linux : `/proc/<pid>/comm` et `pidfd_open`.

use std::sync::mpsc::Sender;

use crate::watch::Event;

/// Vrai si `pid` est vivant et est un process bun (omp tourne sous bun).
pub fn is_bun(pid: i32) -> bool {
    process_name(pid).is_some_and(|name| name == b"bun")
}

#[cfg(target_os = "macos")]
fn process_name(pid: i32) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len() as u32) };
    (len > 0).then(|| buf[..len as usize].to_vec())
}

#[cfg(target_os = "linux")]
fn process_name(pid: i32) -> Option<Vec<u8>> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim().as_bytes().to_vec())
}

/// Envoie `Event::ProcExit(pid)` quand `pid` meurt (tout de suite s'il est déjà mort).
pub fn watch_exit(pid: i32, tx: Sender<Event>) -> std::io::Result<()> {
    std::thread::Builder::new().name(format!("exit-{pid}")).spawn(move || {
        wait_exit(pid);
        let _ = tx.send(Event::ProcExit(pid));
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn wait_exit(pid: i32) {
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return;
        }
        let change = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // Enregistrement seul : échoue (ESRCH) si le process est déjà mort.
        if libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 {
            let mut out: libc::kevent = std::mem::zeroed();
            while libc::kevent(kq, std::ptr::null(), 0, &mut out, 1, std::ptr::null()) < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {}
        }
        libc::close(kq);
    }
}

#[cfg(target_os = "linux")]
fn wait_exit(pid: i32) {
    unsafe {
        let fd = libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32;
        if fd < 0 {
            return;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        while libc::poll(&mut pfd, 1, -1) < 0
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {}
        libc::close(fd);
    }
}
