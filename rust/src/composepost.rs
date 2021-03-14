/*
 * Copyright (C) 2018 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 */

//! Code run server side to "postprocess"
//! a filesystem tree (usually containing mostly RPMs) in
//! order to prepare it as an OSTree commit.

use crate::cxxrsutil::CxxResult;
use anyhow::{anyhow, Context, Result};
use fn_error_context::context;
use openat_ext::OpenatDirExt;
use rayon::prelude::*;
use std::io::{BufRead, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::{
    borrow::Cow,
    io::{self, Read},
};

/* See rpmostree-core.h */
const RPMOSTREE_RPMDB_LOCATION: &str = "usr/share/rpm";

// rpm-ostree uses /home → /var/home by default as generated by our
// rootfs; we don't expect people to change this.  Let's be nice
// and also fixup the $HOME entries generated by `useradd` so
// that `~` shows up as expected in shells, etc.
//
// https://github.com/coreos/fedora-coreos-config/pull/18
// https://pagure.io/workstation-ostree-config/pull-request/121
// https://discussion.fedoraproject.org/t/adapting-user-home-in-etc-passwd/487/6
// https://github.com/justjanne/powerline-go/issues/94
fn postprocess_useradd(rootfs_dfd: &openat::Dir) -> Result<()> {
    let path = Path::new("usr/etc/default/useradd");
    if let Some(f) = rootfs_dfd.open_file_optional(path)? {
        rootfs_dfd.write_file_with(&path, 0o644, |bufw| -> Result<_> {
            let f = io::BufReader::new(&f);
            for line in f.lines() {
                let line = line?;
                if !line.starts_with("HOME=") {
                    bufw.write_all(line.as_bytes())?;
                } else {
                    bufw.write_all(b"HOME=/var/home")?;
                }
                bufw.write_all(b"\n")?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

// We keep hitting issues with the ostree-remount preset not being
// enabled; let's just do this rather than trying to propagate the
// preset everywhere.
fn postprocess_presets(rootfs_dfd: &openat::Dir) -> Result<()> {
    let wantsdir = "usr/lib/systemd/system/multi-user.target.wants";
    rootfs_dfd.ensure_dir_all(wantsdir, 0o755)?;
    for service in &["ostree-remount.service", "ostree-finalize-staged.path"] {
        let target = format!("../{}", service);
        let loc = Path::new(wantsdir).join(service);
        rootfs_dfd.symlink(&loc, target)?;
    }
    Ok(())
}

// We keep hitting issues with the ostree-remount preset not being
// enabled; let's just do this rather than trying to propagate the
// preset everywhere.
fn postprocess_rpm_macro(rootfs_dfd: &openat::Dir) -> Result<()> {
    let rpm_macros_dir = "usr/lib/rpm/macros.d";
    rootfs_dfd.ensure_dir_all(rpm_macros_dir, 0o755)?;
    let rpm_macros_dfd = rootfs_dfd.sub_dir(rpm_macros_dir)?;
    rpm_macros_dfd.write_file_with("macros.rpm-ostree", 0o644, |w| -> Result<()> {
        w.write_all(b"%_dbpath /")?;
        w.write_all(RPMOSTREE_RPMDB_LOCATION.as_bytes())?;
        Ok(())
    })?;
    Ok(())
}

// This function does two things: (1) make sure there is a /home --> /var/home substitution rule,
// and (2) make sure there *isn't* a /var/home -> /home substition rule. The latter check won't
// technically be needed once downstreams have:
// https://src.fedoraproject.org/rpms/selinux-policy/pull-request/14
fn postprocess_subs_dist(rootfs_dfd: &openat::Dir) -> Result<()> {
    let path = Path::new("usr/etc/selinux/targeted/contexts/files/file_contexts.subs_dist");
    if let Some(f) = rootfs_dfd.open_file_optional(path)? {
        rootfs_dfd.write_file_with(&path, 0o644, |w| -> Result<()> {
            let f = io::BufReader::new(&f);
            for line in f.lines() {
                let line = line?;
                if line.starts_with("/var/home ") {
                    w.write_all(b"# https://github.com/projectatomic/rpm-ostree/pull/1754\n")?;
                    w.write_all(b"# ")?;
                }
                w.write_all(line.as_bytes())?;
                w.write_all(b"\n")?;
            }
            w.write_all(b"# https://github.com/projectatomic/rpm-ostree/pull/1754\n")?;
            w.write_all(b"/home /var/home")?;
            w.write_all(b"\n")?;
            Ok(())
        })?;
    }
    Ok(())
}

// This function is called from rpmostree_postprocess_final(); think of
// it as the bits of that function that we've chosen to implement in Rust.
pub(crate) fn compose_postprocess_final(rootfs_dfd: i32) -> CxxResult<()> {
    let rootfs_dfd = crate::ffiutil::ffi_view_openat_dir(rootfs_dfd);
    let tasks = [
        postprocess_useradd,
        postprocess_presets,
        postprocess_subs_dist,
        postprocess_rpm_macro,
    ];
    Ok(tasks.par_iter().try_for_each(|f| f(&rootfs_dfd))?)
}

/// The treefile format has two kinds of postprocessing scripts;
/// there's a single `postprocess-script` as well as inline (anonymous)
/// scripts.  This function executes both kinds in bwrap containers.
pub(crate) fn compose_postprocess_scripts(
    rootfs_dfd: i32,
    treefile: &mut crate::treefile::Treefile,
    unified_core: bool,
) -> CxxResult<()> {
    let rootfs_dfd = crate::ffiutil::ffi_view_openat_dir(rootfs_dfd);

    // Execute the anonymous (inline) scripts.
    for (i, script) in treefile.parsed.postprocess.iter().flatten().enumerate() {
        let binpath = format!("/usr/bin/rpmostree-postprocess-inline-{}", i);
        let target_binpath = &binpath[1..];

        rootfs_dfd.write_file_contents(target_binpath, 0o755, script)?;
        println!("Executing `postprocess` inline script '{}'", i);
        let child_argv = vec![binpath.clone()];
        crate::ffi::bwrap_run_mutable(rootfs_dfd.as_raw_fd(), &binpath, &child_argv, unified_core)?;

        rootfs_dfd.remove_file(target_binpath)?;
    }

    // And the single postprocess script.
    if let Some(postprocess_script) = treefile.get_postprocess_script() {
        let binpath = "/usr/bin/rpmostree-treefile-postprocess-script";
        let target_binpath = &binpath[1..];
        postprocess_script.seek(std::io::SeekFrom::Start(0))?;
        let mut reader = std::io::BufReader::new(postprocess_script);
        rootfs_dfd.write_file_with(target_binpath, 0o755, |w| std::io::copy(&mut reader, w))?;
        println!("Executing postprocessing script");

        let child_argv = vec![binpath.to_string()];
        crate::ffi::bwrap_run_mutable(rootfs_dfd.as_raw_fd(), binpath, &child_argv, unified_core)
            .context("Executing postprocessing script")?;

        rootfs_dfd.remove_file(target_binpath)?;
        println!("Finished postprocessing script");
    }
    Ok(())
}

/// Copy additional files
pub(crate) fn compose_postprocess_add_files(
    rootfs_dfd: i32,
    treefile: &mut crate::treefile::Treefile,
) -> CxxResult<()> {
    let rootfs_dfd = crate::ffiutil::ffi_view_openat_dir(rootfs_dfd);

    // Make a deep copy here because get_add_file_fd() also wants an &mut
    // reference.
    let add_files: Vec<_> = treefile
        .parsed
        .add_files
        .iter()
        .flatten()
        .cloned()
        .collect();
    for (src, dest) in add_files {
        let reldest = dest.trim_start_matches('/');
        if reldest.is_empty() {
            return Err(anyhow!("Invalid add-files destination: {}", dest).into());
        }
        let dest = if reldest.starts_with("etc/") {
            Cow::Owned(format!("usr/{}", reldest))
        } else {
            Cow::Borrowed(reldest)
        };

        println!("Adding file {}", dest);
        let dest = Path::new(&*dest);
        if let Some(parent) = dest.parent() {
            rootfs_dfd.ensure_dir_all(parent, 0o755)?;
        }

        let src = treefile.get_add_file(&src);
        src.seek(std::io::SeekFrom::Start(0))?;
        let mut reader = std::io::BufReader::new(src);
        let mode = reader.get_mut().metadata()?.permissions().mode();
        rootfs_dfd.write_file_with(dest, mode, |w| std::io::copy(&mut reader, w))?;
    }
    Ok(())
}

/// Given a string and a set of possible prefixes, return the split
/// prefix and remaining string, or `None` if no matches.
fn strip_any_prefix<'a, 'b>(s: &'a str, prefixes: &[&'b str]) -> Option<(&'b str, &'a str)> {
    prefixes
        .iter()
        .find_map(|&p| s.strip_prefix(p).map(|r| (p, r)))
}

/// Inject `altfiles` after `files` for `passwd:` and `group:` entries.
fn add_altfiles(buf: &str) -> Result<String> {
    let mut r = String::with_capacity(buf.len());
    for line in buf.lines() {
        let parts = if let Some(p) = strip_any_prefix(line, &["passwd:", "group:"]) {
            p
        } else {
            r.push_str(line);
            r.push('\n');
            continue;
        };
        let (prefix, rest) = parts;
        r.push_str(prefix);

        let mut inserted = false;
        for elt in rest.split_whitespace() {
            // Already have altfiles?  We're done
            if elt == "altfiles" {
                return Ok(buf.to_string());
            }
            // We prefer `files altfiles`
            if !inserted && elt == "files" {
                r.push_str(" files altfiles");
                inserted = true;
            } else {
                r.push(' ');
                r.push_str(elt);
            }
        }
        if !inserted {
            r.push_str(" altfiles");
        }
        r.push('\n');
    }
    Ok(r)
}

/// rpm-ostree currently depends on `altfiles`
#[context("Adding altfiles to /etc/nsswitch.conf")]
pub(crate) fn composepost_nsswitch_altfiles(rootfs_dfd: i32) -> CxxResult<()> {
    let path = "usr/etc/nsswitch.conf";
    let rootfs_dfd = crate::ffiutil::ffi_view_openat_dir(rootfs_dfd);
    let nsswitch = {
        let mut nsswitch = rootfs_dfd.open_file(path)?;
        let mut buf = String::new();
        nsswitch.read_to_string(&mut buf)?;
        buf
    };
    let nsswitch = add_altfiles(&nsswitch)?;
    rootfs_dfd.write_file_contents(path, 0o644, nsswitch.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripany() {
        let s = "foo: bar";
        assert!(strip_any_prefix(s, &[]).is_none());
        assert_eq!(
            strip_any_prefix(s, &["baz:", "foo:", "bar:"]).unwrap(),
            ("foo:", " bar")
        );
    }

    #[test]
    fn altfiles_replaced() {
        let orig = r##"# blah blah nss stuff
# more blah blah

# passwd: db files
# shadow: db files
# shadow: db files

passwd:     sss files systemd
shadow:     files
group:      sss files systemd
hosts:      files resolve [!UNAVAIL=return] myhostname dns
automount:  files sss
"##;
        let expected = r##"# blah blah nss stuff
# more blah blah

# passwd: db files
# shadow: db files
# shadow: db files

passwd: sss files altfiles systemd
shadow:     files
group: sss files altfiles systemd
hosts:      files resolve [!UNAVAIL=return] myhostname dns
automount:  files sss
"##;
        let replaced = add_altfiles(orig).unwrap();
        assert_eq!(replaced.as_str(), expected);
        let replaced2 = add_altfiles(replaced.as_str()).unwrap();
        assert_eq!(replaced2.as_str(), expected);
    }
}
