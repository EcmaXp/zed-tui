use super::*;

#[gpui::test]
async fn test_epoch_ticker_runs_only_while_calls_are_in_flight(cx: &mut TestAppContext) {
    let executor = cx.executor();
    let ticks = Arc::new(AtomicUsize::new(0));
    let (epoch_gate, epoch_ticker) = EpochGate::new(
        {
            let executor = executor.clone();
            move || executor.timer(EPOCH_INTERVAL)
        },
        {
            let ticks = ticks.clone();
            move || {
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        },
    );
    let epoch_ticker = executor.spawn(epoch_ticker);

    executor.advance_clock(Duration::from_secs(1));
    assert_eq!(ticks.load(Ordering::SeqCst), 0, "idle ticker must not tick");

    let in_flight_call = epoch_gate.enter();
    executor.advance_clock(EPOCH_INTERVAL);
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "an in-flight call must get a tick within one interval"
    );
    executor.advance_clock(EPOCH_INTERVAL * 4);
    assert_eq!(ticks.load(Ordering::SeqCst), 5);

    drop(in_flight_call);
    let ticks_at_release = ticks.load(Ordering::SeqCst);
    executor.advance_clock(Duration::from_secs(1));
    assert!(
        ticks.load(Ordering::SeqCst) <= ticks_at_release + 1,
        "ticker must stop after the last call finishes"
    );

    drop(epoch_ticker);
}

#[test]
fn test_epoch_ticker_makes_spinning_wasm_call_yield() {
    const SPIN_MODULE: &[u8] = b"\0asm\x01\0\0\0\
        \x01\x04\x01\x60\0\0\
        \x03\x02\x01\0\
        \x07\x08\x01\x04spin\0\0\
        \x0a\x09\x01\x07\0\x03\x40\x0c\0\x0b\x0b";

    let mut config = wasmtime::Config::new();
    config.epoch_interruption(true);
    let engine = Engine::new(&config).unwrap();
    let module = wasmtime::Module::new(&engine, SPIN_MODULE).unwrap();
    let mut store = Store::new(&engine, ());
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);
    let instance =
        futures::executor::block_on(wasmtime::Instance::new_async(&mut store, &module, &[]))
            .unwrap();
    let spin = instance
        .get_typed_func::<(), ()>(&mut store, "spin")
        .unwrap();

    let ticking_engine = engine.clone();
    let (epoch_gate, epoch_ticker) = EpochGate::new(
        || async { std::thread::sleep(EPOCH_INTERVAL) },
        move || ticking_engine.increment_epoch(),
    );
    std::thread::spawn(move || futures::executor::block_on(epoch_ticker));
    std::thread::sleep(EPOCH_INTERVAL);
    let in_flight_call = epoch_gate.enter();

    let (poll_sender, poll_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut call = std::pin::pin!(spin.call_async(&mut store, ()));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let yielded = call.as_mut().poll(&mut context).is_pending();
        poll_sender.send(yielded).unwrap();
    });

    let poll_result = poll_receiver.recv_timeout(Duration::from_secs(1));
    if poll_result.is_err() {
        engine.increment_epoch();
    }
    drop(in_flight_call);
    assert_eq!(
        poll_result,
        Ok(true),
        "a spinning wasm call must yield once the ticker bumps the epoch"
    );
}
