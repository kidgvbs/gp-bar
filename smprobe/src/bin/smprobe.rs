//! `smprobe` — the probe app binary. Two faces:
//!
//! * **No-args mode (the sampler)** — this is what runs when the .app is
//!   launched via `open`. If the daemon is `NotRegistered` it calls
//!   `SMAppService.register` (this is where the system prompt with Touch ID
//!   appears — the Regular-policy, `open`-launched path that worked on
//!   21/10). Then, forever: every [`identity::SAMPLER_PERIOD_SECS`] record
//!   one line — `SMAppService.status()` + one XPC ping — to
//!   `~/Library/Logs/smprobe.log`. That log is the anomaly-C detector: a
//!   spontaneous `NotFound` gets a timestamp without anyone watching.
//!
//!   The app process runs as the user; writing inside the user's own home
//!   is correct here (the "no user-writable paths for root" rule governs
//!   the daemon side only).
//!
//! * **CLI verbs** — `status`, `ping`, `register`, `unregister`, `log [N]`,
//!   `make-plist <bundle-root>` (the install-time plist generator used by
//!   `scripts/build-probe.sh`, called with the DESTINATION path), `help`.
//!
//!   `register` from a Terminal may return EPERM with no prompt (anomaly
//!   A): the protocol tells the human to prefer the `open`-launched
//!   sampler; the verb exists for completeness.

use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use smprobe::{client, identity, proto, sampler, smapp, status, tail, time};

const USAGE: &str = "smprobe — BTM (SMAppService) stability probe for macOS 27.x

USAGE:
    smprobe                      sampler: register if NotRegistered, then
                                 sample status + one XPC ping every 5 min to
                                 ~/Library/Logs/smprobe.log (runs forever;
                                 this is the mode the .app runs)
    smprobe status               raw SMAppServiceStatus + decoded name
    smprobe ping                 one XPC ping to the daemon
    smprobe register             SMAppService register (prefer `open`ing the
                                 app: the prompt needs a Regular-policy app)
    smprobe unregister           SMAppService unregister
    smprobe log [N]              last N lines of the sampler log (default 100)
    smprobe make-plist <bundle>  print the LaunchDaemon plist for <bundle>
                                 (absolute path; used by scripts/build-probe.sh)
    smprobe help                 this text";

fn main() {
  // Die the Unix way on a closed pipe (`smprobe log | head`), instead of
  // panicking on EPIPE as Rust's print! does by default.
  unsafe {
    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
  }
  let args: Vec<String> = std::env::args().skip(1).collect();
  let code = match args.as_slice() {
    // The sampler is the app's no-args mode; it never returns (`!`).
    [] => run_sampler(),
    [verb, rest @ ..] => match (verb.as_str(), rest) {
      ("status", []) => cmd_status(),
      ("ping", []) => cmd_ping(),
      ("register", []) => cmd_register(),
      ("unregister", []) => cmd_unregister(),
      ("log", []) => cmd_log(identity::LOG_TAIL_DEFAULT),
      ("log", [n]) => match n.parse::<usize>() {
        Ok(n) => cmd_log(n),
        Err(_) => {
          eprintln!("log: N must be a number, got {n:?}");
          2
        }
      },
      ("make-plist", [bundle_root]) => cmd_make_plist(bundle_root),
      ("help", []) | ("--help", []) | ("-h", []) => {
        println!("{USAGE}");
        0
      }
      _ => {
        eprintln!("{USAGE}");
        2
      }
    },
  };
  std::process::exit(code);
}

// -- sampler -----------------------------------------------------------------------

/// Register once if needed, then sample forever. One line per sample,
/// appended to the log; failures are logged, never fatal — the sampler's
/// whole job is to keep observing.
fn run_sampler() -> ! {
  let log = sampler_log_path();
  append_line(
    &log,
    &sampler::event_line(&time::now_utc(), "sampler=started"),
  );

  // The ONLY register call site, and it runs in the `open`-launched
  // (Regular-policy) process, exactly like the known-good 21/10 path.
  let raw = smapp::read_status();
  if raw == status::NOT_REGISTERED {
    eprintln!("smprobe: daemon not registered; calling SMAppService.register");
    let outcome = match smapp::register() {
      Ok(()) => format!(
        "register=ok status={}",
        status::status_line(smapp::read_status())
      ),
      Err(error) => format!("register=error: {error}"),
    };
    append_line(&log, &sampler::event_line(&time::now_utc(), &outcome));
  } else {
    append_line(
      &log,
      &sampler::event_line(
        &time::now_utc(),
        &format!("register=skipped status={}", status::status_line(raw)),
      ),
    );
  }

  loop {
    let line = sampler::sample_line(&time::now_utc(), smapp::read_status(), &client::ping());
    append_line(&log, &line);
    // Also on stderr: visible when launched with `open --stderr <file>`.
    eprintln!("smprobe: {line}");
    thread::sleep(Duration::from_secs(identity::SAMPLER_PERIOD_SECS));
  }
}

/// `~/Library/Logs/smprobe.log`. The app runs as the user; HOME is always
/// set for a GUI-launched process on macOS, and a missing HOME is a loud
/// error, not a silent fallback.
fn sampler_log_path() -> PathBuf {
  let home = std::env::var_os("HOME")
    .unwrap_or_else(|| panic!("smprobe: HOME is not set; cannot resolve the sampler log path"));
  PathBuf::from(home).join(identity::SAMPLER_LOG_RELATIVE)
}

/// Append one line to the log, creating it if needed. Never panics: a log
/// write failure must not kill the sampler.
fn append_line(path: &std::path::Path, line: &str) {
  if let Some(dir) = path.parent() {
    if let Err(error) = std::fs::create_dir_all(dir) {
      eprintln!("smprobe: cannot create {}: {error}", dir.display());
      return;
    }
  }
  let opened = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(path);
  match opened {
    Ok(mut file) => {
      if let Err(error) = writeln!(file, "{line}") {
        eprintln!("smprobe: cannot append to {}: {error}", path.display());
      }
    }
    Err(error) => eprintln!("smprobe: cannot open {}: {error}", path.display()),
  }
}

// -- CLI verbs ------------------------------------------------------------------------

fn cmd_status() -> i32 {
  println!("{}", status::status_line(smapp::read_status()));
  0
}

fn cmd_ping() -> i32 {
  match client::ping() {
    Ok(reply) => {
      println!("ok uid={} pid={} echo={}", reply.uid, reply.pid, reply.echo);
      0
    }
    Err(error) => {
      println!("error: {error}");
      1
    }
  }
}

fn cmd_register() -> i32 {
  match smapp::register() {
    Ok(()) => {
      println!(
        "register: ok — status={}",
        status::status_line(smapp::read_status())
      );
      0
    }
    Err(error) => {
      println!("register failed: {error}");
      1
    }
  }
}

fn cmd_unregister() -> i32 {
  match smapp::unregister() {
    Ok(()) => {
      println!(
        "unregister: ok — status={}",
        status::status_line(smapp::read_status())
      );
      0
    }
    Err(error) => {
      println!("unregister failed: {error}");
      1
    }
  }
}

fn cmd_log(n: usize) -> i32 {
  let path = sampler_log_path();
  match std::fs::read_to_string(&path) {
    Ok(content) => {
      for line in tail::last_lines(&content, n) {
        println!("{line}");
      }
      0
    }
    Err(error) => {
      println!("no sampler log at {}: {error}", path.display());
      0
    }
  }
}

/// Install-time helper: print the daemon plist for the bundle at
/// `bundle_root` (absolute — the DESTINATION path, never the build tree).
/// `scripts/build-probe.sh` writes it into
/// `Contents/Library/LaunchDaemons/` BEFORE the ad-hoc codesign, so the
/// plist is sealed into the bundle signature (anomaly B).
fn cmd_make_plist(bundle_root: &str) -> i32 {
  if !std::path::Path::new(bundle_root).is_absolute() {
    eprintln!("make-plist: the bundle path must be absolute, got {bundle_root:?}");
    return 2;
  }
  // Real check, fail-capable: the plist must point at an existing daemon
  // binary, and the build script must have copied it before calling this.
  let daemon = std::path::Path::new(bundle_root)
    .join("Contents")
    .join("MacOS")
    .join(identity::DAEMON_EXECUTABLE);
  if !daemon.is_file() {
    eprintln!(
      "make-plist: {} is missing; copy the daemon binary into the bundle first",
      daemon.display()
    );
    return 2;
  }
  print!("{}", proto::daemon_plist(bundle_root));
  0
}
