//! Explicit, local selected-word checking. No personal dictionary writes.
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Dictionaries(Vec<String>),
    Suggestions(Vec<String>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unsupported,
    Unavailable,
    Busy,
    Invalid,
    Failed,
}

pub fn valid_word(word: &str) -> bool {
    use unicode_segmentation::UnicodeSegmentation;
    !word.is_empty()
        && word.len() <= 256
        && word.split(['\'', '’', '-']).all(|part| {
            part.chars().any(char::is_alphabetic)
                && !part
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '.' | '/' | '\\'))
                && part.unicode_words().eq(std::iter::once(part))
        })
}
#[cfg(any(target_os = "linux", test))]
fn valid_language(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-@".contains(&b))
}
#[cfg(any(target_os = "linux", test))]
fn parse(bytes: &[u8], dictionaries: bool, original: Option<&str>) -> Result<Reply, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Failed)?;
    if dictionaries {
        let mut result = Vec::new();
        for line in text.lines() {
            let tag = line.split_whitespace().next().ok_or(Error::Failed)?;
            if valid_language(tag) && !result.iter().any(|s| s == tag) && result.len() < 64 {
                result.push(tag.to_owned());
            }
        }
        return if result.is_empty() {
            Err(Error::Unavailable)
        } else {
            Ok(Reply::Dictionaries(result))
        };
    }
    let mut result = Vec::new();
    let mut answers = 0;
    for line in text.lines() {
        if (line.starts_with('&') || line.starts_with('?') || line.starts_with('#'))
            && original.is_some_and(|word| line.split_whitespace().nth(1) != Some(word))
        {
            return Err(Error::Failed);
        }
        if line == "*" || line.starts_with('+') || line.starts_with('-') || line.starts_with('#') {
            answers += 1;
        }
        if line.starts_with('&') || line.starts_with('?') {
            let (_, list) = line.split_once(": ").ok_or(Error::Failed)?;
            answers += 1;
            for word in list.split(", ") {
                if valid_word(word) && result.len() < 8 && !result.iter().any(|s| s == word) {
                    result.push(word.to_owned());
                }
            }
        }
    }
    if answers == 1 {
        Ok(Reply::Suggestions(result))
    } else {
        Err(Error::Failed)
    }
}

/// Dropping this future cancels the owned worker; that worker kills and reaps
/// its child before releasing the single-process slot.
pub async fn request(word: Option<(String, String)>) -> Result<Reply, Error> {
    #[cfg(target_os = "linux")]
    {
        linux::request(word).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = word;
        Err(Error::Unsupported)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    static BUSY: AtomicBool = AtomicBool::new(false);
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            BUSY.store(false, Ordering::Release);
        }
    }
    pub(super) async fn request(word: Option<(String, String)>) -> Result<Reply, Error> {
        if let Some((word, language)) = &word
            && (!valid_word(word) || !valid_language(language))
        {
            return Err(Error::Invalid);
        }
        if BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::Busy);
        }
        let slot = Slot;
        let (mut tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _slot = slot;
            let dictionaries = word.is_none();
            let mut command = tokio::process::Command::new(if dictionaries {
                "enchant-lsmod-2"
            } else {
                "enchant-2"
            });
            if let Some((_, language)) = &word {
                command.args(["-a", "-d", language]);
            } else {
                command.arg("-list-dicts");
            }
            let result = match spawn_owned(&mut command) {
                Err(_) => Err(Error::Unavailable),
                Ok(mut child) => {
                    collect(
                        &mut child,
                        &word,
                        dictionaries,
                        &mut tx,
                        std::time::Duration::from_secs(2),
                    )
                    .await
                }
            };
            drop(_slot);
            let _ = tx.send(result);
        });
        rx.await.unwrap_or(Err(Error::Failed))
    }
    fn spawn_owned(
        command: &mut tokio::process::Command,
    ) -> std::io::Result<tokio::process::Child> {
        // Normal cancellation explicitly kills and reaps in collect. This is
        // the last-resort guard if the runtime tears down the owner task.
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    }

    async fn collect(
        child: &mut tokio::process::Child,
        word: &Option<(String, String)>,
        dictionaries: bool,
        tx: &mut tokio::sync::oneshot::Sender<Result<Reply, Error>>,
        deadline: std::time::Duration,
    ) -> Result<Reply, Error> {
        let operation = async {
            if let Some(mut input) = child.stdin.take()
                && let Some((word, _)) = &word
            {
                input
                    .write_all(format!("^{word}\n").as_bytes())
                    .await
                    .map_err(|_| Error::Failed)?;
            }
            let output = child.stdout.take().ok_or(Error::Failed)?;
            let mut bytes = Vec::new();
            output
                .take(16 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| Error::Failed)?;
            if bytes.len() > 16 * 1024 {
                return Err(Error::Failed);
            }
            if !child.wait().await.map_err(|_| Error::Failed)?.success() {
                return Err(Error::Unavailable);
            }
            parse(
                &bytes,
                dictionaries,
                word.as_ref().map(|(word, _)| word.as_str()),
            )
        };
        let result = tokio::select! {
            _ = tx.closed() => Err(Error::Failed),
            value = tokio::time::timeout(deadline, operation) => value.unwrap_or(Err(Error::Failed)),
        };
        // Also reap on malformed output / I/O errors, not only timeout.
        let _ = child.kill().await;
        let _ = child.wait().await;
        result
    }

    #[cfg(test)]
    mod process_tests {
        use super::*;
        #[test]
        #[allow(unsafe_code)]
        fn runtime_shutdown_kills_its_owned_child() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let pid = runtime.block_on(async {
                let child = spawn_owned(tokio::process::Command::new("sleep").arg("30")).unwrap();
                let pid = child.id().unwrap() as libc::pid_t;
                tokio::spawn(async move {
                    let _child = child;
                    std::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;
                pid
            });
            drop(runtime);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let mut status = 0;
                // SAFETY: pid identifies only this test's own spawned child;
                // status points to live writable storage. Never wait for or
                // signal unrelated processes.
                let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if result == pid {
                    assert!(libc::WIFSIGNALED(status));
                    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
                    break;
                }
                if result == -1 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ECHILD)
                    );
                    break; // Tokio already reaped it.
                }
                if std::time::Instant::now() >= deadline {
                    // SAFETY: waitpid just confirmed this owned child is still
                    // live. Clean it up even when the regression fails.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                        libc::waitpid(pid, &mut status, 0);
                    }
                    panic!("spellcheck child survived runtime shutdown");
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        #[tokio::test]
        async fn cancellation_and_deadline_reap_the_actual_owned_child() {
            for cancel in [false, true] {
                let mut child = tokio::process::Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let (mut tx, rx) = tokio::sync::oneshot::channel();
                let receiver = if cancel {
                    drop(rx);
                    None
                } else {
                    Some(rx)
                };
                assert_eq!(
                    collect(
                        &mut child,
                        &None,
                        true,
                        &mut tx,
                        std::time::Duration::from_millis(10)
                    )
                    .await,
                    Err(Error::Failed)
                );
                assert!(
                    child.try_wait().unwrap().is_some(),
                    "child must already be reaped"
                );
                drop(receiver);
            }
        }
        #[tokio::test]
        async fn protocol_sends_only_prefixed_word_and_eof() {
            let mut child = tokio::process::Command::new("sh").args(["-c", r#"read -r line; test "$line" = '^helo' || exit 4; if read -r extra; then exit 5; fi; printf '*\n'"#])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            let (mut tx, _rx) = tokio::sync::oneshot::channel();
            assert_eq!(
                collect(
                    &mut child,
                    &Some(("helo".into(), "en_US".into())),
                    false,
                    &mut tx,
                    std::time::Duration::from_secs(2)
                )
                .await,
                Ok(Reply::Suggestions(vec![]))
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_bounds_and_unicode() {
        assert!(valid_word("café"));
        assert!(valid_word("cafe\u{301}"));
        assert!(valid_word("привеет"));
        assert!(valid_word("한글"));
        assert!(!valid_word("two words"));
        assert!(!valid_word("hello.world"));
        assert!(
            parse(
                b"& foo 1 1: food\n& bar 1 5: bare\n",
                false,
                Some("foo-bar")
            )
            .is_err()
        );
        assert!(parse(b"*\n*\n", false, Some("foo-bar")).is_err());
        assert!(!valid_word("*save\n"));
        assert!(!valid_word(&"a".repeat(257)));
        assert_eq!(
            parse(b"en_US (aspell)\nen_US (hunspell)\n", true, None),
            Ok(Reply::Dictionaries(vec!["en_US".into()]))
        );
        assert_eq!(
            parse(
                b"@(#) banner\n& helo 2 1: hello, help\n\n",
                false,
                Some("helo")
            ),
            Ok(Reply::Suggestions(vec!["hello".into(), "help".into()]))
        );
        assert!(parse(b"garbage", false, Some("helo")).is_err());
        assert!(parse(&[255], false, Some("helo")).is_err());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod installed_tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires installed Enchant and en_US dictionary"]
    async fn installed_enchant_checks_real_word() {
        let Reply::Dictionaries(languages) = request(None).await.unwrap() else {
            panic!("dictionary reply")
        };
        assert!(languages.iter().any(|language| language == "en_US"));
        let Reply::Suggestions(words) = request(Some(("helo".into(), "en_US".into())))
            .await
            .unwrap()
        else {
            panic!("suggestion reply")
        };
        assert!(words.iter().any(|word| word == "hello"));
    }
}
