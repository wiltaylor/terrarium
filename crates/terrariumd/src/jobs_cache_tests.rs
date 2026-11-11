// Test-only, path-scoped faults. No process-global environment changes, and
// no hook is compiled into a release daemon.
type PrepareFault = Arc<dyn Fn(&git::JobRepo) + Send + Sync>;

fn prepare_faults() -> &'static Mutex<HashMap<PathBuf, PrepareFault>> {
    static FAULTS: OnceLock<Mutex<HashMap<PathBuf, PrepareFault>>> = OnceLock::new();
    FAULTS.get_or_init(Mutex::default)
}

fn prepare_fault(repo: &git::JobRepo) {
    let fault = prepare_faults().lock().unwrap().get(repo.path()).cloned();
    if let Some(fault) = fault {
        fault(repo);
    }
}

fn cleanup_faults() -> &'static Mutex<HashMap<PathBuf, std::io::ErrorKind>> {
    static FAULTS: OnceLock<Mutex<HashMap<PathBuf, std::io::ErrorKind>>> = OnceLock::new();
    FAULTS.get_or_init(Mutex::default)
}

fn cleanup_fault(path: &std::path::Path) -> std::io::Result<()> {
    match cleanup_faults().lock().unwrap().remove(path) {
        Some(kind) => Err(std::io::Error::new(kind, "injected cleanup failure")),
        None => Ok(()),
    }
}

struct CacheFixture {
    src: Src,
    daemon: crate::Daemon,
    repo: Arc<git::JobRepo>,
    job: Job,
    target: terrarium_core::config::TargetDef,
    started: AtomicBool,
    lost: OnceLock<String>,
    step: Mutex<&'static str>,
}

impl CacheFixture {
    async fn new() -> Self {
        let src = Src::new();
        let (head, _) = src.commit("cache-test");
        git_in(&src.src, &["branch", "next"]);
        let root = src.tmp.path();
        let daemon = crate::start(crate::DaemonConfig {
            socket: root.join("daemon.sock"),
            listen: None,
            token: None,
            state_dir: root.join("state"),
            config_dir: root.join("config"),
            work_dir: root.join("work"),
            max_targets_per_project: crate::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
            repos_dir: root.join("repos"),
            supervisor: Arc::new(crate::supervisor::InMemorySupervisor::new(
                Duration::from_millis(20),
            )),
            insecure_agent_header: true,
            artifact_listen: None,
            web: crate::settings::WebConfig::default(),
        })
        .await
        .unwrap();
        let repo = Arc::new(
            git::JobRepo::create(git::Checkout {
                url: src.src.display().to_string(),
                dest: job_repo_dir(&daemon.state, "p", "app"),
                branch: None,
                depth: None,
                secret: None,
                store: RunDirs::new(&daemon.state.state_dir),
                deny_local_under: vec![],
                timeout: Duration::from_secs(10),
                hooks: git::Hooks::default(),
                origin: git::Origin::default(),
                fetch_from: None,
            })
            .unwrap(),
        );
        let mut job = ended(JobKind::Validate, jobs::JobState::Running, None);
        job.id = root.display().to_string();
        job.branch = "main".into();
        job.commit = Some(head);
        job.slot = Some(1);
        Self {
            src,
            daemon,
            repo,
            job,
            target: serde_json::from_value(serde_json::json!({"kind": "host"})).unwrap(),
            started: AtomicBool::new(false),
            lost: OnceLock::new(),
            step: Mutex::new(STEP_SETUP),
        }
    }

    async fn ctx(&self) -> StepCtx<'_> {
        let locks = repo_lock(self.repo.path());
        let lifetime = locks.lifetime.clone().read_owned().await;
        StepCtx {
            state: &self.daemon.state,
            job: &self.job,
            def: &self.target,
            dir: "/unused",
            repo: self.repo.clone(),
            lifetime: Some(Arc::new(lifetime)),
            repo_locks: locks,
            bundle: self.src.tmp.path().join("job.bundle"),
            holder: None,
            ionice: false,
            recipe_started: &self.started,
            recipe_lost: &self.lost,
            step: &self.step,
            ssh_host: None,
            slot: 1,
            wipe: false,
        }
    }
}

#[tokio::test]
async fn ter275_cancelled_daemon_work_keeps_cache_and_damage_marker_until_it_ends() {
    let fixture = Arc::new(CacheFixture::new().await);
    let path = fixture.repo.path().to_owned();
    std::fs::write(path.join("keep-until-user-ends"), "kept").unwrap();
    let (ready, paused) = tokio::sync::oneshot::channel();
    let (release, go) = std::sync::mpsc::channel();
    let user = fixture.clone();
    let task = tokio::spawn(async move {
        let ctx = user.ctx().await;
        ctx.daemon(move |repo| {
            std::fs::create_dir_all(repo.path().join("refs/job")).unwrap();
            std::fs::write(repo.path().join("refs/job/broken"), "invalid").unwrap();
            assert!(
                repo.fetch(&["+refs/heads/main:refs/remotes/job/head".into()])
                    .is_err()
            );
            assert!(repo.path().join("job-damaged").exists());
            ready.send(()).unwrap();
            go.recv_timeout(Duration::from_secs(10)).unwrap();
            // The real blocking user still needs the old repository.
            assert!(repo.path().join("keep-until-user-ends").exists());
        })
        .await
        .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(10), paused)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    let _ = task.await;
    assert!(
        path.join("job-damaged").exists(),
        "cancellation lost recovery work"
    );
    let locks = repo_lock(&path);
    let repo = fixture.repo.clone();
    let (queued, waiting) = tokio::sync::oneshot::channel();
    let rebuild = tokio::spawn(async move {
        let _prepare = locks.prepare.clone().lock_owned().await;
        queued.send(()).unwrap();
        let _exclusive = locks.lifetime.clone().write_owned().await;
        repo.rebuild().unwrap();
    });
    waiting.await.unwrap();
    // A queued writer must not delete the cache while detached daemon work
    // is paused. With the production clone removed this writer completes.
    let mut rebuild = rebuild;
    let early = tokio::time::timeout(Duration::from_millis(200), &mut rebuild)
        .await
        .is_ok();
    release.send(()).unwrap();
    if !early {
        tokio::time::timeout(Duration::from_secs(10), rebuild)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        !early,
        "rebuild deleted a repository still used by StepCtx::daemon"
    );
    assert!(!path.join("keep-until-user-ends").exists());
    assert!(!path.join("job-damaged").exists());
}

#[tokio::test]
async fn ter275_a_second_damaged_prepare_ends_without_a_second_rebuild() {
    let fixture = CacheFixture::new().await;
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = attempts.clone();
    prepare_faults().lock().unwrap().insert(
        fixture.repo.path().to_owned(),
        Arc::new(move |repo| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            assert!(
                !repo.path().join("attempt-sentinel").exists(),
                "a previous cache was not rebuilt"
            );
            std::fs::write(repo.path().join("attempt-sentinel"), n.to_string()).unwrap();
            std::fs::create_dir_all(repo.path().join("refs/job")).unwrap();
            std::fs::write(repo.path().join("refs/job/broken"), "invalid").unwrap();
        }),
    );
    let mut ctx = fixture.ctx().await;
    let guard = ctx.repo_locks.prepare.clone().lock_owned().await;
    let result = steps(
        &mut ctx,
        "must not run".into(),
        &DeliveryDef::default(),
        guard,
    )
    .await
    .unwrap();
    prepare_faults().lock().unwrap().remove(fixture.repo.path());
    assert!(!result.passed);
    assert!(result.tail.unwrap().contains("cannot fetch"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read_to_string(fixture.repo.path().join("attempt-sentinel")).unwrap(),
        "1"
    );
    assert!(fixture.repo.path().join("job-damaged").exists());
}

#[test]
fn ter275_cleanup_returns_first_error_but_removes_the_next_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("p");
    std::fs::create_dir(&project).unwrap();
    for name in ["one.bundle", "two.bundle", "three.bundle"] {
        std::fs::write(project.join(name), "interrupted").unwrap();
    }
    // Pick read_dir's first entry rather than relying on filesystem order.
    let paths: Vec<_> = std::fs::read_dir(&project)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    cleanup_faults()
        .lock()
        .unwrap()
        .insert(paths[0].clone(), std::io::ErrorKind::PermissionDenied);
    cleanup_faults()
        .lock()
        .unwrap()
        .insert(paths[1].clone(), std::io::ErrorKind::Other);
    let error = cleanup_job_files(tmp.path()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!paths[2].exists(), "cleanup stopped after an earlier error");
    assert!(paths[0].exists());
    assert!(paths[1].exists());
}
