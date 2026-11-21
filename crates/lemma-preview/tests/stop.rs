//! `stop_all`, which the application calls as it ends: it is for the whole process, so it has a
//! test binary of its own. A shell script stands in for pdflatex, so no TeX is needed.

#[cfg(unix)]
#[test]
fn stop_all_kills_the_runs_going_on_and_refuses_new_ones() {
    use lemma_preview::{FormulaKind, Job, PreambleSource, Previewer, project_key, stop_all};
    use std::path::Path;
    use std::time::{Duration, Instant};

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-preview-{}-stop", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A "pdflatex" that never ends by itself, as TeX does on a macro that expands to itself.
    let pid_file = dir.join("tex.pid");
    let program = dir.join("pdflatex");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
            pid_file.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cache = dir.join("cache");
    let mut previewer = Previewer::new(&cache);
    previewer.pdflatex = program.into_os_string();
    previewer.format_timeout = Duration::from_secs(60);
    let job = Job {
        root: None,
        dir: dir.clone(),
        preamble: PreambleSource::Default,
        definitions: String::new(),
        kind: FormulaKind::Inline,
        formula: "x".into(),
        scale: 2.0,
    };
    let started = Instant::now();
    let previewer = std::sync::Arc::new(previewer);
    let render = {
        let (previewer, job) = (previewer.clone(), job.clone());
        std::thread::spawn(move || previewer.render(&job))
    };
    // The format build has started.
    while !pid_file.is_file()
        || std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .is_empty()
    {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the run started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_owned();
    assert!(stop_all(Duration::from_secs(10)), "the render returned");
    assert!(render.is_finished());
    let outcome = render.join().unwrap();
    assert!(outcome.result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the render came back at once"
    );
    // The process is gone (killed and reaped by the run).
    #[cfg(target_os = "linux")]
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "TeX ended");
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    // A format build that was killed is not remembered as one TeX refused, and no TeX run was
    // set up after it.
    let project = cache.join(project_key(None));
    let left: Vec<_> = std::fs::read_dir(&project)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "left: {left:?}");
    // And nothing runs any more.
    std::fs::remove_file(&pid_file).unwrap();
    let outcome = previewer.render(&job);
    assert!(outcome.result.is_err());
    assert!(!pid_file.exists(), "no TeX run after stop_all");
}
