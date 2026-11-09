//! `wang vm setup`: download Firecracker and a guest kernel (checksums pinned), build the VM image
//! from a Docker image with the guest agent inside, and save a just-booted base snapshot.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use wang_jail::Jail;

use crate::{Assets, Snapshot, Vm, VmCfg};

const FC_VERSION: &str = "v1.17.0";
const FC_TGZ_SHA256: &str = "06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558";
const KERNEL_URL: &str = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/x86_64/vmlinux-6.1.155";
const KERNEL_SHA256: &str = "e20e46d0c36c55c0d1014eb20576171b3f3d922260d9f792017aeff53af3d4f2";
pub const DEFAULT_IMAGE: &str = "python:3.12-slim";

pub struct SetupOpts {
    pub root: PathBuf,
    pub image: String,
    pub agent: Option<PathBuf>,
    pub cfg: VmCfg,
    /// Rebuild the image and base snapshot even if they exist.
    pub rebuild: bool,
}

pub fn sha256(p: &Path) -> Result<String> {
    let out = Command::new("sha256sum").arg(p).output().context("running sha256sum")?;
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("").to_string())
}

/// Download `url` to `dest` unless it's already there with the right checksum; verify before keeping.
pub async fn download(url: &str, dest: &Path, want_sha: &str, log: &dyn Fn(&str)) -> Result<()> {
    if dest.exists() && sha256(dest)? == want_sha {
        return Ok(());
    }
    log(&format!("downloading {url}"));
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    let got = sha256(&tmp)?;
    if got != want_sha {
        let _ = std::fs::remove_file(&tmp);
        bail!("checksum mismatch for {url}: got {got}, expected {want_sha}");
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

fn run(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The static guest agent: `WANG_VM_AGENT`, or the static build next to this executable.
pub fn find_agent() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("WANG_VM_AGENT") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("exe dir")?;
    for c in [
        dir.join("../x86_64-unknown-linux-gnu/release/wang-vm-agent"),
        dir.join("../../x86_64-unknown-linux-gnu/release/wang-vm-agent"),
        dir.join("wang-vm-agent"),
    ] {
        if c.is_file() && is_static(&c) {
            return Ok(c);
        }
    }
    bail!(
        "the static guest agent isn't built. Build it with:\n  RUSTFLAGS=\"-C target-feature=+crt-static\" cargo build --release --target x86_64-unknown-linux-gnu -p wang-vm-agent"
    )
}

/// No ELF interpreter = statically linked (runs in any image).
fn is_static(p: &Path) -> bool {
    std::fs::read(p).map(|b| !b.windows(15).take(4096).any(|w| w == b"/lib64/ld-linux")).unwrap_or(false)
}

pub async fn setup(jail: &Jail, opts: &SetupOpts, log: &dyn Fn(&str)) -> Result<Assets> {
    let a = Assets::at(&opts.root);
    let dl = opts.root.join("dl");
    std::fs::create_dir_all(&dl)?;
    if !Path::new("/dev/kvm").exists() {
        bail!("/dev/kvm not found: enable virtualization (VT-x/AMD-V) and KVM first");
    }
    std::fs::File::options().read(true).write(true).open("/dev/kvm").context("can't open /dev/kvm (add yourself to the kvm group)")?;

    // 1. Firecracker.
    if !a.firecracker.exists() {
        let tgz = dl.join(format!("firecracker-{FC_VERSION}-x86_64.tgz"));
        let url = format!("https://github.com/firecracker-microvm/firecracker/releases/download/{FC_VERSION}/firecracker-{FC_VERSION}-x86_64.tgz");
        download(&url, &tgz, FC_TGZ_SHA256, log).await?;
        run(Command::new("tar").arg("xzf").arg(&tgz).arg("-C").arg(&dl), "tar")?;
        std::fs::copy(dl.join(format!("release-{FC_VERSION}-x86_64/firecracker-{FC_VERSION}-x86_64")), &a.firecracker)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&a.firecracker, std::fs::Permissions::from_mode(0o755))?;
    }
    log(&format!("firecracker: {}", run(Command::new(&a.firecracker).arg("--version"), "firecracker")?.lines().next().unwrap_or("?")));

    // 2. Kernel (Firecracker's CI build: virtio, vsock, overlayfs, squashfs, VMGenID).
    download(KERNEL_URL, &a.kernel, KERNEL_SHA256, log).await?;
    log("kernel: linux 6.1.155 (firecracker CI build)");

    // 3. Image: Docker image → squashfs, with the agent as /wang-agent (PID 1).
    let agent = match &opts.agent {
        Some(p) => p.clone(),
        None => find_agent()?,
    };
    let stamp = format!("{} {}", opts.image, sha256(&agent)?);
    let stamp_file = opts.root.join("rootfs.stamp");
    let fresh = a.rootfs.exists() && std::fs::read_to_string(&stamp_file).ok().as_deref() == Some(stamp.as_str());
    if opts.rebuild || !fresh {
        log(&format!("building the VM image from docker image {}", opts.image));
        run(Command::new("docker").args(["pull", "-q", &opts.image]), "docker pull")?;
        let id = run(Command::new("docker").args(["create", &opts.image]), "docker create")?;
        let tmp = a.rootfs.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let script = format!(
            "docker export {id} | mksquashfs - '{}' -tar -noappend -comp zstd -quiet -p 'wang-agent f 755 0 0 cat {}' -p 'wang d 755 0 0'",
            tmp.display(),
            agent.display()
        );
        let r = run(Command::new("sh").arg("-c").arg(&script), "docker export | mksquashfs");
        let _ = Command::new("docker").args(["rm", &id]).output();
        r?;
        std::fs::rename(&tmp, &a.rootfs)?;
        std::fs::write(&stamp_file, &stamp)?;
        let _ = std::fs::remove_dir_all(a.base_dir());
    }
    log(&format!("image: {} ({} MiB)", opts.image, std::fs::metadata(&a.rootfs)?.len() >> 20));

    // 4. Base snapshot: boot once, save the idle machine; new VMs restore it instead of booting.
    if opts.rebuild || a.base(&opts.cfg).is_none() {
        let _ = std::fs::remove_dir_all(a.base_dir());
        let mut vm = Vm::boot(jail, &a, &opts.cfg).await?;
        log(&format!("cold boot: {:.0} ms to a ready agent", vm.ready_ms));
        let snap = vm.snapshot(&a, true).await;
        vm.stop().await;
        let snap = snap?;
        std::fs::rename(&snap.dir, a.base_dir())?;
        let base = Snapshot { dir: a.base_dir(), ..snap };
        base.save_meta()?;
        log(&format!("base snapshot: {} vCPU, {} MiB, saved in {:.0} ms", base.cfg.vcpus, base.cfg.mem_mib, base.create_ms));
    }
    Ok(a)
}
