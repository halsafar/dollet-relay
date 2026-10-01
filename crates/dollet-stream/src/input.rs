//! Where bytes come from: a provider's HTTP response, or a command's stdout.

use std::process::Stdio;
use std::time::Duration;

use futures_util::TryStreamExt;
use tokio::io::AsyncRead;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio_util::io::StreamReader;
use uuid::Uuid;

use crate::StreamError;

/// One upstream the channel can be served from, in the order the caller wants
/// them tried. `id` is the caller's stream id; this crate only echoes it back
/// in stats and in log lines.
#[derive(Debug, Clone)]
pub struct StreamSource {
    pub id: i64,
    pub url: String,
    pub user_agent: String,
    pub profile: SourceProfile,
    pub limit: Option<SourceLimit>,
}

/// How the source is consumed. `Redirect` is not a transport: it means the
/// server should answer the client with a 302 and stay out of the data path
/// entirely. The decision belongs to `dollet-server`, but it is modelled here
/// because it is a property of the stream profile attached to the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceProfile {
    Proxy,
    Redirect,
    Command { command: String, parameters: String },
}

/// The provider account (or M3U profile) whose concurrent-connection budget
/// this source spends.
#[derive(Debug, Clone, Copy)]
pub struct SourceLimit {
    pub key: i64,
    pub max_streams: u32,
}

/// A transcode applied to a channel's raw ring, producing the ring an
/// `OutputKey::Profile` session serves. Must read MPEG-TS on stdin and write
/// it on stdout.
#[derive(Debug, Clone)]
pub struct Transcode {
    pub command: String,
    pub parameters: String,
}

/// Substitutes the placeholders a stream profile's parameters may contain and
/// splits them the way a shell would, so quoted arguments survive.
pub fn build_command(
    command: &str,
    parameters: &str,
    url: &str,
    user_agent: &str,
    channel: Uuid,
) -> Result<Vec<String>, StreamError> {
    if command.trim().is_empty() {
        return Err(StreamError::BadCommand("empty command".into()));
    }
    let channel = channel.to_string();
    let parts =
        shell_words::split(parameters).map_err(|e| StreamError::BadCommand(e.to_string()))?;

    let mut argv = Vec::with_capacity(parts.len() + 1);
    argv.push(command.to_owned());
    argv.extend(parts.into_iter().map(|part| {
        part.replace("{streamUrl}", url)
            .replace("{userAgent}", user_agent)
            .replace("{channelId}", &channel)
    }));
    Ok(argv)
}

pub struct Spawned {
    pub child: Child,
    pub stdout: ChildStdout,
    pub stdin: Option<ChildStdin>,
    pub stderr: ChildStderr,
}

/// Kill and reap. `kill_on_drop` gets there eventually, but a failover must
/// release the provider's connection *before* opening the next one, or the
/// account momentarily holds two and trips its own stream limit.
///
/// The signal goes to the whole process group, because a stream profile is
/// frequently a pipeline — `sh -c "streamlink ... | ffmpeg ..."` — and killing
/// only the shell leaves the producing half holding the provider's socket
/// after this process has already released the connection guard that was
/// accounting for it. The symptom of that leak is the *provider* refusing new
/// connections, which does not look like a bug on this side.
pub async fn terminate(mut child: Child) {
    #[cfg(unix)]
    if !kill_process_group(&child) {
        // No group left to signal — or the process never got one. The direct
        // child is still ours.
        let _ = child.start_kill();
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();

    let _ = child.wait().await;
}

/// SIGKILL to the child's process group. Returns false when there was no group
/// to signal, so the caller can fall back to the child alone.
#[cfg(unix)]
fn kill_process_group(child: &Child) -> bool {
    // Must be read before any reap: an already-waited child has no id, and by
    // then the group is gone with it.
    let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }

    // SAFETY: an FFI call with no memory operands. The delivery target is what
    // needs care: `spawn` makes every command its own group leader, so the
    // group id equals this pid, and the positive check above means it can
    // never widen to this process's own group (0) or to every process (-1).
    unsafe { libc::killpg(pid, libc::SIGKILL) == 0 }
}

pub fn spawn(argv: &[String], with_stdin: bool) -> Result<Spawned, StreamError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| StreamError::BadCommand("empty command".into()))?;

    let mut cmd = Command::new(program);
    // Its own process group: it gives `terminate` a single handle on every
    // process the command spawns behind itself, and it keeps a signal aimed at
    // this server from racing the supervisor into the children.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if with_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // A dropped supervisor must not leave an ffmpeg pulling from a
        // provider forever; that is how a connection budget gets exhausted by
        // processes nobody is watching.
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| StreamError::Upstream(format!("{program}: {e}")))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdin = child.stdin.take();
    Ok(Spawned {
        child,
        stdout,
        stdin,
        stderr,
    })
}

pub async fn open_http(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    connect_timeout: Duration,
) -> Result<impl AsyncRead + Send + Unpin + use<>, StreamError> {
    let request = client
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent)
        .send();

    let response = tokio::time::timeout(connect_timeout, request)
        .await
        .map_err(|_| StreamError::Upstream("connection timed out".into()))?
        .map_err(|e| StreamError::Upstream(e.to_string()))?
        .error_for_status()
        .map_err(|e| StreamError::Upstream(e.to_string()))?;

    let body = response
        .bytes_stream()
        .map_err(|e| std::io::Error::other(e.to_string()));
    Ok(StreamReader::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_placeholders_and_respects_quoting() {
        let channel = Uuid::nil();
        let argv = build_command(
            "ffmpeg",
            r#"-user_agent "{userAgent}" -i {streamUrl} -metadata title="chan {channelId}" -f mpegts pipe:1"#,
            "http://example/live.ts",
            "VLC/3.0.20 LibVLC/3.0.20",
            channel,
        )
        .expect("built");

        assert_eq!(
            argv,
            vec![
                "ffmpeg",
                "-user_agent",
                "VLC/3.0.20 LibVLC/3.0.20",
                "-i",
                "http://example/live.ts",
                "-metadata",
                &format!("title=chan {channel}"),
                "-f",
                "mpegts",
                "pipe:1",
            ]
        );
    }

    #[test]
    fn rejects_an_empty_command() {
        let err = build_command("  ", "-i x", "u", "a", Uuid::nil()).unwrap_err();
        assert!(matches!(err, StreamError::BadCommand(_)));
    }

    #[test]
    fn rejects_unbalanced_quotes() {
        let err = build_command("ffmpeg", "-i \"unclosed", "u", "a", Uuid::nil()).unwrap_err();
        assert!(matches!(err, StreamError::BadCommand(_)));
    }

    #[test]
    fn parameters_may_be_empty() {
        let argv = build_command("cat", "", "u", "a", Uuid::nil()).expect("built");
        assert_eq!(argv, vec!["cat"]);
    }

    /// Needs `sh` and `sleep`, as the rest of this crate's process tests do.
    /// There is no way to produce a grandchild without a program that forks,
    /// and a test that only checked the direct child would pass whether or not
    /// the group was signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_reaches_a_grandchild_the_command_left_behind() {
        use tokio::io::AsyncReadExt;

        // The shell backgrounds a long sleep, reports its pid, and exits --
        // exactly the shape of `streamlink | ffmpeg` where the producing half
        // outlives the shell and keeps the provider's socket open.
        let argv = build_command(
            "sh",
            "-c 'sleep 30 >/dev/null 2>&1 & echo $!'",
            "",
            "",
            Uuid::nil(),
        )
        .expect("built");
        let mut spawned = spawn(&argv, false).expect("spawned");

        let mut reported = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            spawned.stdout.read_to_string(&mut reported),
        )
        .await
        .expect("shell never reported a pid")
        .expect("read");
        let orphan: i32 = reported.trim().parse().expect("a pid");

        // Signal 0 alone would count a zombie as alive. The orphan's new
        // parent is whatever PID 1 is, and in a container that is often a
        // program that never reaps — so a killed grandchild lingers as `Z`
        // indefinitely. Where /proc is readable, trust its state instead.
        let alive = || match std::fs::read_to_string(format!("/proc/{orphan}/stat")) {
            Ok(stat) => !stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z')),
            // SAFETY: signal 0 only probes for existence; it delivers nothing.
            Err(_) => unsafe { libc::kill(orphan, 0) == 0 },
        };
        assert!(alive(), "the grandchild was never running");

        terminate(spawned.child).await;

        // Without the group signal the orphan would sleep out the full thirty
        // seconds.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while alive() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "grandchild {orphan} outlived its group"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn spawning_a_missing_program_is_an_upstream_error() {
        let err = spawn(&["dollet-no-such-program".to_owned()], false)
            .err()
            .expect("spawn should fail");
        assert!(matches!(err, StreamError::Upstream(_)));
        assert!(spawn(&[], false).is_err());
    }
}
