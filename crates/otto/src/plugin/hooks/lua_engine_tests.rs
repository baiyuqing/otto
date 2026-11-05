use super::*;
use std::time::Duration;
fn evaluate(source: &str) -> Result<Value, String> {
    run(
        source.as_bytes(),
        br#"{"arguments":{"number":7,"empty":[],"null":null}}"#,
        Instant::now() + Duration::from_secs(1),
        Arc::new(AtomicBool::new(false)),
    )
    .and_then(|result| serde_json::from_str(&result).map_err(|error| error.to_string()))
}
#[test]
fn json_values_and_fresh_vm() {
    assert_eq!(
        evaluate("local input=...; return {content=tostring(input.arguments.number * 6)}").unwrap(),
        serde_json::json!({"content":"42"})
    );
    assert_eq!(
        evaluate("local input=...; return input.arguments").unwrap(),
        serde_json::json!({"number":7,"empty":[],"null":null})
    );
    assert_eq!(
        evaluate("return {empty=json.array({}), null=json.null}").unwrap(),
        serde_json::json!({"empty":[],"null":null})
    );
    evaluate("persisted=42; return {}").unwrap();
    assert_eq!(
        evaluate("return {absent=persisted==nil}").unwrap(),
        serde_json::json!({"absent":true})
    );
}
#[test]
fn host_capabilities_are_absent_and_attempts_fail_without_echo() {
    for name in [
        "io",
        "os",
        "package",
        "require",
        "debug",
        "ffi",
        "load",
        "loadfile",
        "dofile",
        "print",
        "warn",
        "collectgarbage",
        "coroutine",
        "string",
        "table",
        "pcall",
        "xpcall",
        "setmetatable",
        "getmetatable",
    ] {
        assert_eq!(
            evaluate(&format!("return {{absent={name}==nil}}")).unwrap(),
            serde_json::json!({"absent":true}),
            "{name}"
        );
    }
    for source in [
        "return io.open('/etc/passwd')",
        "return os.execute('touch malicious-file')",
        "return os.getenv('HOME')",
        "return require('socket')",
        "return require('ffi')",
        "return debug.getregistry()",
        "return load('return 42')()",
        "return dofile('/etc/passwd')",
        "return pcall(function() while true do end end)",
        "return setmetatable({}, {__gc=function() while true do end end})",
        "error('sensitive-attacker-chosen-error')",
        "error({})",
    ] {
        let error = evaluate(source).unwrap_err();
        assert!(!error.contains("sensitive-attacker-chosen-error"));
        assert!(!error.contains("/etc/passwd"));
    }
}
#[test]
fn malicious_computation_and_serialization_are_bounded() {
    for source in [
        "while true do end",
        "local x='x'; while true do x=x..x end",
        "local x={}; x.x=x; return x",
        "return {x=function() end}",
        "local x={}; return {a=x,b=x}",
        "return {[1000000000]=1}",
        "return {[1]=1, x=2}",
        "return {x=0/0}",
        "return {[false]=1}",
        "return {x='\\255'}",
        "local x={}; local root=x; for i=1,100 do x.x={}; x=x.x end; return root",
        "local x={}; for i=1,10000 do x[i]=i end; return x",
        "local x='x'; for i=1,17 do x=x..x end; return {content=x}",
    ] {
        assert!(evaluate(source).is_err(), "{source}");
    }
    assert!(
        run(
            b"while true do end",
            b"{}",
            Instant::now(),
            Arc::new(AtomicBool::new(false))
        )
        .is_err()
    );
    assert!(
        run(
            b"\x1bLua",
            b"{}",
            Instant::now() + Duration::from_secs(1),
            Arc::new(AtomicBool::new(false))
        )
        .is_err()
    );
}
#[test]
fn pre_entry_cancellation_rejects_bounded_sources() {
    for source in [
        "while true do end".to_owned(),
        "-- parsing\n".repeat(20000) + "return {}",
        "local x={}; for i=1,8000 do x[i]=i end; return x".to_owned(),
    ] {
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = stopped.clone();
        let (finished, receive) = std::sync::mpsc::channel();
        stopped.store(true, Ordering::Relaxed);
        let worker = std::thread::spawn(move || {
            let result = run(
                source.as_bytes(),
                b"{}",
                Instant::now() + Duration::from_secs(1),
                signal,
            );
            finished.send(result).unwrap(); // run returned and its VM was destroyed.
        });
        let result = receive
            .recv_timeout(Duration::from_secs(2))
            .expect("bounded worker must finish, not detach");
        assert!(result.is_err());
        worker.join().unwrap();
    }
}
#[test]
fn instruction_hook_observes_cancellation_of_running_vm_and_worker_exits() {
    let stopped = Arc::new(AtomicBool::new(false));
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let signal = stopped.clone();
    let progress = ticks.clone();
    let (finished, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        finished
            .send(execute(
                b"while true do end",
                b"{}",
                Instant::now() + Duration::from_secs(1),
                signal,
                progress,
                None,
            ))
            .unwrap();
    });
    let wait = Instant::now() + Duration::from_secs(1);
    while ticks.load(Ordering::Relaxed) == 0 && Instant::now() < wait {
        std::thread::yield_now();
    }
    assert!(
        ticks.load(Ordering::Relaxed) > 0,
        "VM must execute instructions before cancellation"
    );
    stopped.store(true, Ordering::Relaxed);
    assert!(
        receive
            .recv_timeout(Duration::from_secs(2))
            .expect("VM and destructor must terminate")
            .is_err()
    );
    worker.join().unwrap();
}
#[test]
fn input_and_output_conversion_checkpoints_and_size_limits() {
    let stopped = AtomicBool::new(true);
    let deadline = Instant::now() + Duration::from_secs(1);
    let input = serde_json::json!({"x": [1,2,3]});
    assert!(check_input(&input, 0, &mut NODES.clone(), &stopped, deadline).is_err());
    assert!(
        check_input(
            &input,
            0,
            &mut NODES.clone(),
            &AtomicBool::new(false),
            Instant::now()
        )
        .is_err()
    );
    let lua = Lua::new_with(StdLib::MATH, mlua::LuaOptions::default()).unwrap();
    let mut budget = Budget {
        nodes: NODES,
        bytes: super::super::MAX_OUTPUT,
        seen: HashSet::new(),
        stopped: &stopped,
        deadline,
    };
    assert!(output(&lua, LuaValue::Nil, 0, &mut budget).is_err());
    let huge_input = serde_json::to_vec(&vec![0; NODES + 1]).unwrap();
    assert!(
        run(
            b"return {}",
            &huge_input,
            Instant::now() + Duration::from_secs(1),
            Arc::new(AtomicBool::new(false))
        )
        .is_err()
    );
    assert!(
        run(
            b"return {}",
            &vec![b' '; super::super::MAX_INPUT + 1],
            Instant::now() + Duration::from_secs(1),
            Arc::new(AtomicBool::new(false))
        )
        .is_err()
    );
    assert!(
        run(
            &vec![b' '; MAX_SOURCE as usize + 1],
            b"{}",
            Instant::now() + Duration::from_secs(1),
            Arc::new(AtomicBool::new(false))
        )
        .is_err()
    );
    for source in [b"return {}".as_slice(), b"not valid Lua", b"\x1bLua"] {
        assert!(
            run(
                source,
                b"{}",
                Instant::now(),
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
    }
}
