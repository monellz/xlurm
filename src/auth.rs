//! Local identities come from the kernel, never from submitted JSON.
use crate::model::Owner;
use anyhow::{Context, Result, ensure};
use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

pub fn peer(socket: &UnixStream) -> Result<(u32, u32)> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0 && length as usize == std::mem::size_of_val(&credentials),
        "cannot authenticate Unix socket peer"
    );
    Ok((credentials.uid, credentials.gid))
}

pub fn account(uid: u32, gid: u32) -> Result<Owner> {
    let mut buffer = vec![0u8; 16384];
    loop {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        let result = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if result == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        ensure!(
            result == 0 && !found.is_null(),
            "no local account for UID {uid}"
        );
        return Ok(Owner {
            uid,
            gid,
            name: unsafe { CStr::from_ptr(entry.pw_name) }
                .to_str()
                .context("non-UTF8 account name")?
                .to_owned(),
        });
    }
}

pub fn groups(owner: &Owner) -> Result<Vec<libc::gid_t>> {
    let name = CString::new(owner.name.as_str())?;
    let mut count = 16;
    loop {
        ensure!(
            count > 0 && count <= 65536,
            "invalid supplementary group count"
        );
        let mut groups = vec![0; count as usize];
        let previous = count;
        let result = unsafe {
            libc::getgrouplist(name.as_ptr(), owner.gid, groups.as_mut_ptr(), &mut count)
        };
        if result >= 0 {
            groups.truncate(count as usize);
            return Ok(groups);
        }
        ensure!(count > previous, "cannot resolve supplementary groups");
    }
}

pub fn authorize(uid: u32, owner: &Owner) -> Result<()> {
    ensure!(
        uid == 0 || uid == owner.uid,
        "permission denied: job belongs to {} (UID {})",
        owner.name,
        owner.uid
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owners_and_root_can_manage_jobs_but_other_users_cannot() {
        let owner = Owner {
            uid: 1234,
            gid: 4567,
            name: "alice".into(),
        };
        assert!(authorize(1234, &owner).is_ok());
        assert!(authorize(0, &owner).is_ok());
        assert!(authorize(1235, &owner).is_err());
    }

    #[test]
    fn socket_identity_is_the_kernel_identity() {
        let (one, _two) = UnixStream::pair().unwrap();
        assert_eq!(
            peer(&one).unwrap(),
            (unsafe { libc::geteuid() }, unsafe { libc::getegid() })
        );
    }
}
