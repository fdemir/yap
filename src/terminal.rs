use std::io::{self, stdout};

use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
};

pub struct TerminalSession {
    terminal: ratatui::DefaultTerminal,
    _restore: RestoreGuard<fn()>,
}

impl TerminalSession {
    pub fn start() -> io::Result<Self> {
        let terminal = ratatui::try_init()?;
        let restore = RestoreGuard::new(restore_terminal as fn());
        execute!(stdout(), EnableBracketedPaste)?;
        Ok(Self {
            terminal,
            _restore: restore,
        })
    }

    pub fn terminal_mut(&mut self) -> &mut ratatui::DefaultTerminal {
        &mut self.terminal
    }

    pub fn restore_now(&mut self) {
        self._restore.restore_now();
    }
}

fn restore_terminal() {
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
}

struct RestoreGuard<F: FnOnce()> {
    restore: Option<F>,
}

impl<F: FnOnce()> RestoreGuard<F> {
    fn new(restore: F) -> Self {
        Self {
            restore: Some(restore),
        }
    }

    fn restore_now(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
    }
}

impl<F: FnOnce()> Drop for RestoreGuard<F> {
    fn drop(&mut self) {
        self.restore_now();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::RestoreGuard;

    #[test]
    fn cleanup_runs_on_normal_return_error_and_panic() {
        let cleanups = Arc::new(AtomicUsize::new(0));

        {
            let cleanups = cleanups.clone();
            let _guard = RestoreGuard::new(move || {
                cleanups.fetch_add(1, Ordering::SeqCst);
            });
        }

        let error: Result<(), ()> = {
            let cleanups = cleanups.clone();
            let _guard = RestoreGuard::new(move || {
                cleanups.fetch_add(1, Ordering::SeqCst);
            });
            Err(())
        };
        assert_eq!(error, Err(()));

        let panic = catch_unwind(AssertUnwindSafe({
            let cleanups = cleanups.clone();
            move || {
                let _guard = RestoreGuard::new(move || {
                    cleanups.fetch_add(1, Ordering::SeqCst);
                });
                panic!("fixture panic");
            }
        }));
        assert!(panic.is_err());
        assert_eq!(cleanups.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn early_cleanup_only_runs_once() {
        let cleanups = Arc::new(AtomicUsize::new(0));
        {
            let guard_cleanups = cleanups.clone();
            let mut guard = RestoreGuard::new(move || {
                guard_cleanups.fetch_add(1, Ordering::SeqCst);
            });
            guard.restore_now();
            guard.restore_now();
        }

        assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cleanup_runs_when_a_task_is_cancelled() {
        let cleanups = Arc::new(AtomicUsize::new(0));
        let task_cleanups = cleanups.clone();
        let task = tokio::spawn(async move {
            let _guard = RestoreGuard::new(move || {
                task_cleanups.fetch_add(1, Ordering::SeqCst);
            });
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        task.abort();
        let _ = task.await;

        assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    }
}
