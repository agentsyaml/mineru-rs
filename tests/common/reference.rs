use std::{
    env,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use url::Url;

pub fn required(name: &str) -> String {
    let value =
        env::var(name).unwrap_or_else(|_| panic!("{name} is required when MINERU_RUN_REFERENCE=1"));
    assert!(!value.trim().is_empty(), "{name} is empty");
    value
}

pub fn reference_env() -> (String, String, Option<String>) {
    assert_eq!(
        required("MINERU_RUN_REFERENCE"),
        "1",
        "MINERU_RUN_REFERENCE must be 1"
    );
    let url = required("MINERU_REFERENCE_URL");
    let parsed = Url::parse(&url).expect("MINERU_REFERENCE_URL is invalid");
    assert!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "MINERU_REFERENCE_URL must not contain credentials"
    );
    (
        url,
        required("MINERU_REFERENCE_MODEL"),
        env::var("MINERU_REFERENCE_BEARER_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
    )
}

pub fn run(command: &mut Command, name: &str, timeout: Duration, token: Option<&str>) {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            unsafe extern "C" {
                fn setpgid(pid: i32, pgid: i32) -> i32;
            }
            if setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("cannot start {name}: {e}"));
    #[cfg(unix)]
    let pid = child.id() as i32;
    let mut stderr = child.stderr.take().expect("piped stderr missing");
    let reader = thread::spawn(move || {
        const LIMIT: usize = 16 * 1024;
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        let mut truncated = false;
        loop {
            let read = stderr.read(&mut chunk).expect("cannot read child stderr");
            if read == 0 {
                break;
            }
            let keep = (LIMIT - bytes.len()).min(read);
            bytes.extend_from_slice(&chunk[..keep]);
            truncated |= keep != read;
        }
        if truncated {
            bytes.extend_from_slice(b"\n[stderr truncated]");
        }
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|e| panic!("cannot wait for {name}: {e}"))
        {
            let stderr = reader.join().expect("stderr reader thread panicked");
            assert!(
                status.success(),
                "{name} failed: {status}: {}",
                redact(stderr, token)
            );
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    #[cfg(unix)]
    unsafe {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // Dedicated group ensures grandchildren do not survive a timed-out inference.
        let _ = kill(-pid, 15);
        thread::sleep(Duration::from_millis(200));
        let _ = kill(-pid, 9);
    }
    #[cfg(not(unix))]
    child
        .kill()
        .unwrap_or_else(|e| panic!("cannot stop timed out {name}: {e}"));
    child
        .wait()
        .unwrap_or_else(|e| panic!("cannot reap timed out {name}: {e}"));
    let _ = reader.join();
    panic!("{name} timed out after {} seconds", timeout.as_secs());
}

fn redact(value: String, token: Option<&str>) -> String {
    token
        .filter(|token| !token.is_empty())
        .map_or(value.clone(), |token| value.replace(token, "REDACTED"))
}

pub fn pinned_venv(temp: &Path, token: Option<&str>) -> PathBuf {
    let venv = temp.join("venv");
    run(
        Command::new("uv").args(["venv"]).arg(&venv),
        "uv venv",
        Duration::from_secs(300),
        token,
    );
    let python = venv.join("bin/python");
    run(
        Command::new("uv")
            .args(["pip", "install", "--python"])
            .arg(&python)
            .args([
                required("MINERU_REFERENCE_MINERU_WHEEL"),
                required("MINERU_REFERENCE_VL_UTILS_WHEEL"),
            ]),
        "pinned MinerU install",
        Duration::from_secs(300),
        token,
    );
    run(Command::new(&python).args(["-c", "import importlib.metadata as m; assert m.version('mineru') == '3.4.4'; assert m.version('mineru-vl-utils') == '1.0.5'"]), "pinned package verification", Duration::from_secs(300), token);
    python
}
