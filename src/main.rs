use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{self, Command};

#[cfg(unix)]
use std::process::{Child, Stdio};

extern crate isatty;
use isatty::{stderr_isatty, stdout_isatty};

#[cfg(unix)]
extern crate tempfile;

fn main() {
    let result = cargo_expand_or_run_nightly();
    process::exit(match result {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(&mut io::stderr(), "{}", err);
            1
        }
    });
}

fn cargo_expand_or_run_nightly() -> io::Result<i32> {
    const NO_RUN_NIGHTLY: &str = "CARGO_EXPAND_NO_RUN_NIGHTLY";

    let maybe_nightly = !definitely_not_nightly();
    if maybe_nightly || env::var_os(NO_RUN_NIGHTLY).is_some() {
        return cargo_expand();
    }

    let mut nightly = Command::new("cargo");
    nightly.arg("+nightly");
    nightly.arg("expand");
    nightly.args(env::args_os().skip(1));

    // Hopefully prevent infinite re-run loop.
    nightly.env(NO_RUN_NIGHTLY, "");

    let status = nightly.status()?;

    Ok(match status.code() {
        Some(code) => code,
        None => if status.success() { 0 } else { 1 },
    })
}

fn definitely_not_nightly() -> bool {
    let mut cmd = Command::new(cargo_binary());
    cmd.arg("--version");

    let output = match cmd.output() {
        Ok(output) => output,
        Err(_) => return false,
    };

    let version = match String::from_utf8(output.stdout) {
        Ok(version) => version,
        Err(_) => return false,
    };

    version.starts_with("cargo 1") && !version.contains("nightly")
}

fn cargo_binary() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| "cargo".to_owned().into())
}

#[cfg(windows)]
fn cargo_expand() -> io::Result<i32> {
    // Build cargo command
    let mut cmd = Command::new(cargo_binary());
    cmd.args(&wrap_args(env::args_os(), None));
    run(cmd)
}

#[cfg(unix)]
fn cargo_expand() -> io::Result<i32> {
    let args: Vec<_> = env::args_os().collect();
    match args.last().unwrap().to_str().unwrap_or("") {
        "--filter-cargo" => filter_err(ignore_cargo_err),
        "--filter-rustfmt" => filter_err(ignore_rustfmt_err),
        _ => {}
    }

    macro_rules! shell {
        ($($arg:expr)*) => {
            &[$(OsStr::new(&$arg)),*]
        };
    }

    let which_rustfmt = which(&["rustfmt"]);
    let which_pygmentize = if !color_never(&args) && stdout_isatty() {
        which(&["pygmentize", "-l", "rust"])
    } else {
        None
    };

    let outdir = if which_rustfmt.is_some() || which_pygmentize.is_some() {
        let mut builder = tempfile::Builder::new();
        builder.prefix("cargo-expand");
        Some(builder.tempdir().expect("failed to create tmp file"))
    } else {
        None
    };
    let outfile = outdir.as_ref().map(|dir| dir.path().join("expanded"));

    // Build cargo command
    let mut cmd = Command::new(cargo_binary());
    cmd.args(&wrap_args(args.clone(), outfile.as_ref()));

    // Pipe to a tmp file to separate out any println output from build scripts
    if let Some(outfile) = outfile {
        let mut filter_cargo = Vec::new();
        filter_cargo.extend(args.iter().map(OsString::as_os_str));
        filter_cargo.push(OsStr::new("--filter-cargo"));

        let _wait = cmd.pipe_to(shell!("cat"), Some(&filter_cargo))?;
        run(cmd)?;
        drop(_wait);

        cmd = Command::new("cat");
        cmd.arg(outfile);
    }

    // Pipe to rustfmt
    let _wait = match which_rustfmt {
        Some(ref fmt) => {
            let args: Vec<_> = env::args_os().collect();
            let mut filter_rustfmt = Vec::new();
            filter_rustfmt.extend(args.iter().map(OsString::as_os_str));
            filter_rustfmt.push(OsStr::new("--filter-rustfmt"));

            Some((
                cmd.pipe_to(shell!(fmt), None)?,
                cmd.pipe_to(shell!("cat"), Some(&filter_rustfmt))?,
            ))
        }
        None => None,
    };

    // Pipe to pygmentize
    let _wait = match which_pygmentize {
        Some(pyg) => Some(cmd.pipe_to(shell!(pyg "-l" "rust" "-O" "encoding=utf8"), None)?),
        None => None,
    };

    run(cmd)
}

fn run(mut cmd: Command) -> io::Result<i32> {
    cmd.status().map(|status| status.code().unwrap_or(1))
}

#[cfg(unix)]
struct Wait(Vec<Child>);

#[cfg(unix)]
impl Drop for Wait {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if let Err(err) = child.wait() {
                let _ = writeln!(&mut io::stderr(), "{}", err);
            }
        }
    }
}

#[cfg(unix)]
trait PipeTo {
    fn pipe_to(&mut self, out: &[&OsStr], err: Option<&[&OsStr]>) -> io::Result<Wait>;
}

#[cfg(unix)]
impl PipeTo for Command {
    fn pipe_to(&mut self, out: &[&OsStr], err: Option<&[&OsStr]>) -> io::Result<Wait> {
        use std::os::unix::io::{AsRawFd, FromRawFd};

        self.stdout(Stdio::piped());
        if err.is_some() {
            self.stderr(Stdio::piped());
        }

        let child = self.spawn()?;

        *self = Command::new(out[0]);
        self.args(&out[1..]);
        self.stdin(unsafe {
            Stdio::from_raw_fd(child.stdout.as_ref().map(AsRawFd::as_raw_fd).unwrap())
        });

        match err {
            None => Ok(Wait(vec![child])),
            Some(err) => {
                let mut errcmd = Command::new(err[0]);
                errcmd.args(&err[1..]);
                errcmd.stdin(unsafe {
                    Stdio::from_raw_fd(child.stderr.as_ref().map(AsRawFd::as_raw_fd).unwrap())
                });
                errcmd.stdout(Stdio::null());
                errcmd.stderr(Stdio::inherit());
                let spawn = errcmd.spawn()?;
                Ok(Wait(vec![spawn, child]))
            }
        }
    }
}

// Based on https://github.com/rsolomo/cargo-check
fn wrap_args<I>(it: I, outfile: Option<&PathBuf>) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = vec!["rustc".into()];
    let mut ends_with_test = false;
    let mut ends_with_example = false;
    let mut has_color = false;

    let mut it = it.into_iter().skip(2);
    for arg in &mut it {
        if arg == *"--" {
            break;
        }
        ends_with_test = arg == *"--test";
        ends_with_example = arg == *"--example";
        has_color |= arg.to_str().unwrap_or("").starts_with("--color");
        args.push(arg.into());
    }

    if ends_with_test {
        // Expand the `test.rs` test by default.
        args.push("test".into());
    }

    if ends_with_example {
        // Expand the `example.rs` example by default.
        args.push("example".into());
    }

    if !has_color {
        let color = stderr_isatty();
        let setting = if color { "always" } else { "never" };
        args.push(format!("--color={}", setting).into());
    }

    args.push("--".into());
    if let Some(path) = outfile {
        args.push("-o".into());
        args.push(path.into());
    }
    args.push("-Zunstable-options".into());
    args.push("--pretty=expanded".into());
    args.extend(it);
    args
}

fn color_never(args: &Vec<OsString>) -> bool {
    args.windows(2).any(|pair| pair[0] == *"--color" && pair[1] == *"never")
        || args.iter().any(|arg| *arg == *"--color=never")
}

#[cfg(unix)]
fn which(cmd: &[&str]) -> Option<OsString> {
    if env::args_os().find(|arg| arg == "--help").is_some() {
        return None;
    }

    if let Some(which) = env::var_os(&cmd[0].to_uppercase()) {
        return if which.is_empty() { None } else { Some(which) };
    }

    let spawn = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(_) => {
            return None;
        }
    };

    let exit = match child.wait() {
        Ok(exit) => exit,
        Err(_) => {
            return None;
        }
    };

    if exit.success() {
        Some(cmd[0].into())
    } else {
        None
    }
}

#[cfg(unix)]
fn filter_err(ignore: fn(&str) -> bool) -> ! {
    let mut line = String::new();
    while let Ok(n) = io::stdin().read_line(&mut line) {
        if n == 0 {
            break;
        }
        if !ignore(&line) {
            let _ = write!(&mut io::stderr(), "{}", line);
        }
        line.clear();
    }
    process::exit(0);
}

#[cfg(unix)]
fn ignore_rustfmt_err(_line: &str) -> bool {
    true
}

#[cfg(unix)]
fn ignore_cargo_err(line: &str) -> bool {
    if line.trim().is_empty() {
        return true;
    }

    let blacklist = [
        "ignoring specified output filename because multiple outputs were \
         requested",
        "ignoring specified output filename for 'link' output because multiple \
         outputs were requested",
        "ignoring --out-dir flag due to -o flag.",
        "due to multiple output types requested, the explicitly specified \
         output file name will be adapted for each output type",
    ];
    for s in &blacklist {
        if line.contains(s) {
            return true;
        }
    }

    false
}
