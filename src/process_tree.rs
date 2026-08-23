#[cfg(unix)]
mod platform {
    use std::{
        collections::HashSet,
        io, thread,
        time::{Duration, Instant},
    };

    #[cfg(target_os = "macos")]
    use std::ffi::{c_int, c_void};
    #[cfg(target_os = "linux")]
    use std::{fs, path::PathBuf};

    use rustix::process::{Pid, Signal, WaitOptions};
    use tokio::sync::{Mutex, MutexGuard};

    const MAX_TRACKED_PROCESSES: usize = 4096;
    #[cfg(target_os = "linux")]
    const MAX_LINUX_CHILDREN_BYTES: usize = 64 * 1024;
    const TERMINATION_GRACE: Duration = Duration::from_millis(800);
    const FORCE_CLEANUP_WAIT: Duration = Duration::from_millis(250);
    const CLEANUP_POLL: Duration = Duration::from_millis(10);

    static COMMAND_LOCK: Mutex<()> = Mutex::const_new(());

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    enum Identity {
        #[cfg(target_os = "linux")]
        LinuxStartTicks(u64),
        #[cfg(target_os = "macos")]
        MacOsUniqueId(u64),
    }

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    struct TrackedProcess {
        pid: i32,
        identity: Identity,
    }

    #[derive(Clone, Copy)]
    struct ProcessSnapshot {
        identity: Identity,
        parent_pid: i32,
        #[cfg(target_os = "macos")]
        parent_unique_id: u64,
    }

    pub(crate) struct CommandSupervisor {
        _permit: MutexGuard<'static, ()>,
        baseline: HashSet<TrackedProcess>,
    }

    impl CommandSupervisor {
        pub(crate) async fn start() -> Self {
            let permit = COMMAND_LOCK.lock().await;
            enable_child_subreaper();
            let baseline = direct_children(current_pid())
                .unwrap_or_default()
                .into_iter()
                .collect();
            Self {
                _permit: permit,
                baseline,
            }
        }

        pub(crate) fn track(&self, pid: Option<u32>) -> ProcessTreeGuard {
            ProcessTreeGuard::new(pid, self.baseline.clone())
        }
    }

    pub(crate) struct ProcessTreeGuard {
        group: Option<Pid>,
        tracker: ProcessTracker,
        armed: bool,
    }

    impl ProcessTreeGuard {
        fn new(pid: Option<u32>, baseline: HashSet<TrackedProcess>) -> Self {
            let raw_pid = pid.and_then(|pid| i32::try_from(pid).ok());
            Self {
                group: raw_pid.and_then(Pid::from_raw),
                tracker: ProcessTracker::new(raw_pid, baseline),
                armed: true,
            }
        }

        pub(crate) fn refresh(&mut self) {
            self.tracker.refresh();
        }

        pub(crate) fn leader_finished(&mut self) {
            self.refresh();
            self.signal_all(Signal::KILL);
            self.wait_until_gone(FORCE_CLEANUP_WAIT);
        }

        pub(crate) fn disarm(&mut self) {
            self.refresh();
            self.tracker.reap_descendants();
            self.armed = false;
            self.group = None;
        }

        fn signal_all(&mut self, signal: Signal) {
            if let Some(group) = self.group {
                let _ = rustix::process::kill_process_group(group, signal);
            }
            self.tracker.signal_all(signal);
        }

        fn any_alive(&self) -> bool {
            let group_alive = self
                .group
                .is_some_and(|group| rustix::process::test_kill_process_group(group).is_ok());
            group_alive || self.tracker.any_alive()
        }

        fn terminate_with_grace(&mut self) {
            self.refresh();
            self.signal_all(Signal::TERM);
            self.wait_until_gone(TERMINATION_GRACE);
            if self.any_alive() {
                self.refresh();
                self.signal_all(Signal::KILL);
                self.wait_until_gone(FORCE_CLEANUP_WAIT);
            }
            self.tracker.reap_descendants();
        }

        fn wait_until_gone(&mut self, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                self.tracker.reap_descendants();
                self.refresh();
                if !self.any_alive() {
                    return;
                }
                thread::sleep(CLEANUP_POLL);
            }
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            if self.armed {
                self.terminate_with_grace();
            }
        }
    }

    struct ProcessTracker {
        root: Option<TrackedProcess>,
        processes: Vec<TrackedProcess>,
        baseline: HashSet<TrackedProcess>,
        supervisor: Option<TrackedProcess>,
    }

    impl ProcessTracker {
        fn new(root_pid: Option<i32>, baseline: HashSet<TrackedProcess>) -> Self {
            let root = root_pid.and_then(tracked_process);
            Self {
                root,
                processes: Vec::new(),
                baseline,
                supervisor: tracked_process(current_pid()),
            }
        }

        fn refresh(&mut self) {
            let mut parents = Vec::new();
            if let Some(root) = self.root
                && identity_matches(root)
            {
                parents.push(root);
            }
            parents.extend(
                self.processes
                    .iter()
                    .copied()
                    .filter(|process| identity_matches(*process)),
            );

            if let Some(supervisor) = self.supervisor
                && identity_matches(supervisor)
            {
                self.append_children(supervisor, &mut parents, true);
            }

            let mut index = 0;
            while index < parents.len() && self.processes.len() < MAX_TRACKED_PROCESSES {
                let parent = parents[index];
                index += 1;
                self.append_children(parent, &mut parents, false);
            }

            #[cfg(target_os = "macos")]
            self.refresh_macos_lineage();
        }

        fn append_children(
            &mut self,
            parent: TrackedProcess,
            parents: &mut Vec<TrackedProcess>,
            exclude_baseline: bool,
        ) {
            let Ok(children) = direct_children(parent.pid) else {
                return;
            };
            for child in children {
                if exclude_baseline && self.baseline.contains(&child) {
                    continue;
                }
                if self.insert(child) {
                    parents.push(child);
                }
            }
        }

        fn insert(&mut self, process: TrackedProcess) -> bool {
            if self.root == Some(process)
                || self.baseline.contains(&process)
                || self.processes.contains(&process)
                || self.processes.len() >= MAX_TRACKED_PROCESSES
            {
                return false;
            }
            self.processes.retain(|known| known.pid != process.pid);
            self.processes.push(process);
            true
        }

        #[cfg(target_os = "macos")]
        fn refresh_macos_lineage(&mut self) {
            let mut changed = true;
            while changed && self.processes.len() < MAX_TRACKED_PROCESSES {
                changed = false;
                for pid in all_macos_pids() {
                    if pid <= 0 || pid == current_pid() {
                        continue;
                    }
                    let Ok(snapshot) = capture_snapshot(pid) else {
                        continue;
                    };
                    let parent_is_tracked = self
                        .root
                        .into_iter()
                        .chain(self.processes.iter().copied())
                        .any(|process| match process.identity {
                            Identity::MacOsUniqueId(unique_id) => {
                                unique_id == snapshot.parent_unique_id
                            }
                        });
                    if parent_is_tracked
                        && self.insert(TrackedProcess {
                            pid,
                            identity: snapshot.identity,
                        })
                    {
                        changed = true;
                    }
                }
            }
        }

        fn signal_all(&self, signal: Signal) {
            for process in self.processes.iter().rev().copied() {
                signal_if_current(process, signal);
            }
            if let Some(root) = self.root {
                signal_if_current(root, signal);
            }
        }

        fn any_alive(&self) -> bool {
            self.root.is_some_and(identity_matches)
                || self.processes.iter().copied().any(identity_matches)
        }

        fn reap_descendants(&self) {
            for process in self.processes.iter().copied() {
                let Some(pid) = Pid::from_raw(process.pid) else {
                    continue;
                };
                while let Ok(Some(_)) = rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {}
            }
        }
    }

    fn tracked_process(pid: i32) -> Option<TrackedProcess> {
        capture_snapshot(pid).ok().map(|snapshot| TrackedProcess {
            pid,
            identity: snapshot.identity,
        })
    }

    fn identity_matches(process: TrackedProcess) -> bool {
        capture_snapshot(process.pid).is_ok_and(|snapshot| snapshot.identity == process.identity)
    }

    fn signal_if_current(process: TrackedProcess, signal: Signal) {
        if !identity_matches(process) {
            return;
        }
        if let Some(pid) = Pid::from_raw(process.pid) {
            let _ = rustix::process::kill_process(pid, signal);
        }
    }

    fn direct_children(parent_pid: i32) -> io::Result<Vec<TrackedProcess>> {
        platform_direct_children(parent_pid)
    }

    #[cfg(target_os = "linux")]
    fn platform_direct_children(parent_pid: i32) -> io::Result<Vec<TrackedProcess>> {
        let mut children = Vec::new();
        let task_directory = PathBuf::from(format!("/proc/{parent_pid}/task"));
        for task in fs::read_dir(task_directory)? {
            let task = task?;
            let path = task.path().join("children");
            let bytes = fs::read(path)?;
            if bytes.len() > MAX_LINUX_CHILDREN_BYTES {
                continue;
            }
            for raw_pid in String::from_utf8_lossy(&bytes).split_whitespace() {
                let Ok(pid) = raw_pid.parse::<i32>() else {
                    continue;
                };
                let Ok(snapshot) = capture_snapshot(pid) else {
                    continue;
                };
                if snapshot.parent_pid == parent_pid {
                    children.push(TrackedProcess {
                        pid,
                        identity: snapshot.identity,
                    });
                }
            }
        }
        children.sort_unstable_by_key(|process| process.pid);
        children.dedup();
        Ok(children)
    }

    #[cfg(target_os = "macos")]
    fn platform_direct_children(parent_pid: i32) -> io::Result<Vec<TrackedProcess>> {
        let reported = unsafe { proc_listchildpids(parent_pid, std::ptr::null_mut(), 0) };
        if reported <= 0 {
            return Ok(Vec::new());
        }
        let capacity = usize::try_from(reported)
            .unwrap_or(0)
            .saturating_add(256)
            .min(MAX_TRACKED_PROCESSES);
        let mut pids = vec![0_i32; capacity];
        let bytes =
            c_int::try_from(pids.len().saturating_mul(size_of::<i32>())).unwrap_or(c_int::MAX);
        let count = unsafe { proc_listchildpids(parent_pid, pids.as_mut_ptr().cast(), bytes) };
        if count <= 0 {
            return Ok(Vec::new());
        }
        let count = usize::try_from(count).unwrap_or(0).min(pids.len());
        Ok(pids[..count]
            .iter()
            .copied()
            .filter_map(|pid| {
                let snapshot = capture_snapshot(pid).ok()?;
                (snapshot.parent_pid == parent_pid).then_some(TrackedProcess {
                    pid,
                    identity: snapshot.identity,
                })
            })
            .collect())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn platform_direct_children(_parent_pid: i32) -> io::Result<Vec<TrackedProcess>> {
        Ok(Vec::new())
    }

    #[cfg(target_os = "linux")]
    fn capture_snapshot(pid: i32) -> io::Result<ProcessSnapshot> {
        let stat = fs::read(format!("/proc/{pid}/stat"))?;
        if stat.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process stat is too large",
            ));
        }
        let close_paren = stat.iter().rposition(|byte| *byte == b')').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "process stat has no command")
        })?;
        let fields = String::from_utf8_lossy(&stat[close_paren + 1..]);
        let mut fields = fields.split_whitespace();
        let _state = fields.next();
        let parent_pid = fields
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process has no parent"))?
            .parse::<i32>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let start_ticks = fields
            .nth(17)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process has no start time"))?
            .parse::<u64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(ProcessSnapshot {
            identity: Identity::LinuxStartTicks(start_ticks),
            parent_pid,
        })
    }

    #[cfg(target_os = "macos")]
    fn capture_snapshot(pid: i32) -> io::Result<ProcessSnapshot> {
        let mut unique = ProcUniqueIdentifierInfo::default();
        let unique_size = c_int::try_from(size_of::<ProcUniqueIdentifierInfo>()).unwrap();
        let unique_read = unsafe {
            proc_pidinfo(
                pid,
                PROC_PID_UNIQUE_IDENTIFIER_INFO,
                0,
                (&raw mut unique).cast(),
                unique_size,
            )
        };
        if unique_read != unique_size {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process identity is unavailable",
            ));
        }

        let mut info = ProcBsdInfo::default();
        let info_size = c_int::try_from(size_of::<ProcBsdInfo>()).unwrap();
        let info_read =
            unsafe { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), info_size) };
        if info_read != info_size {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process metadata is unavailable",
            ));
        }

        Ok(ProcessSnapshot {
            identity: Identity::MacOsUniqueId(unique.p_uniqueid),
            parent_pid: i32::try_from(info.pbi_ppid).unwrap_or(i32::MAX),
            parent_unique_id: unique.p_puniqueid,
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn capture_snapshot(_pid: i32) -> io::Result<ProcessSnapshot> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process identity tracking is unsupported",
        ))
    }

    fn current_pid() -> i32 {
        rustix::process::getpid().as_raw_nonzero().get()
    }

    #[cfg(target_os = "linux")]
    fn enable_child_subreaper() {
        let _ = rustix::process::set_child_subreaper(Pid::from_raw(1));
    }

    #[cfg(not(target_os = "linux"))]
    fn enable_child_subreaper() {}

    #[cfg(target_os = "macos")]
    fn all_macos_pids() -> Vec<i32> {
        let reported = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
        if reported <= 0 {
            return Vec::new();
        }
        let capacity = usize::try_from(reported)
            .unwrap_or(0)
            .saturating_add(256)
            .min(65_536);
        let mut pids = vec![0_i32; capacity];
        let bytes =
            c_int::try_from(pids.len().saturating_mul(size_of::<i32>())).unwrap_or(c_int::MAX);
        let count = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let count = usize::try_from(count.max(0)).unwrap_or(0).min(pids.len());
        pids.truncate(count);
        pids
    }

    #[cfg(target_os = "macos")]
    const PROC_PIDTBSDINFO: c_int = 3;
    #[cfg(target_os = "macos")]
    const PROC_PID_UNIQUE_IDENTIFIER_INFO: c_int = 17;

    #[cfg(target_os = "macos")]
    #[repr(C)]
    #[derive(Default)]
    struct ProcUniqueIdentifierInfo {
        p_uuid: [u8; 16],
        p_uniqueid: u64,
        p_puniqueid: u64,
        p_idversion: i32,
        p_orig_ppidversion: i32,
        p_reserve2: u64,
        p_reserve3: u64,
    }

    #[cfg(target_os = "macos")]
    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [u8; 16],
        pbi_name: [u8; 32],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn proc_listchildpids(ppid: c_int, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        fn current_process_identity_is_stable() {
            let process =
                tracked_process(current_pid()).expect("current process should be visible");

            assert!(identity_matches(process));
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::process::Stdio;

    pub(crate) struct CommandSupervisor;

    impl CommandSupervisor {
        pub(crate) async fn start() -> Self {
            Self
        }

        pub(crate) fn track(&self, pid: Option<u32>) -> ProcessTreeGuard {
            ProcessTreeGuard { pid }
        }
    }

    pub(crate) struct ProcessTreeGuard {
        pid: Option<u32>,
    }

    impl ProcessTreeGuard {
        pub(crate) fn refresh(&mut self) {}

        pub(crate) fn leader_finished(&mut self) {}

        pub(crate) fn disarm(&mut self) {
            self.pid = None;
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            if let Some(pid) = self.pid {
                let pid = pid.to_string();
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", pid.as_str(), "/T", "/F"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

pub(crate) use platform::{CommandSupervisor, ProcessTreeGuard};
