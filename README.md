# Icicle benchmarking harness

## Usage

First compile the `bench-harness` and `agent` binaries:

```
cargo build --release
```

Next build the disk images needed to run the benchmarks. (Currently this requires root to allow the disk images to be temporarily mounted during creation).

```
sudo ./target/release/bench-harness build
```

The tool can then be used to run a new benchmark:

```
./target/release/bench-harness --workers=1 bench --id=001 --trials=1 'task-1,task-2,...,task-n'
```

## Benchmarks for Icicle paper

* Instrumentation test benchmark:

```
./target/release/bench-harness --workers=50 bench --id=001 --trials=5 "$(cat data/config/instrumentation-tasks)"
```

* `LAVA-M` bug finding benchmarks:

```
./target/release/bench-harness --workers=50 bench --id=001 --trials=5 "$(cat data/config/lava-tasks)"
```

* `MSP430` benchmarks:

```
./target/release/bench-harness --workers=50 bench --id=001 --trials=5 "$(cat data/config/msp430-tasks)"
```


## Additional benchmarks

* `afl-ghidra-bridge` comparison:

```
./target/release/bench-harness --workers=50 bench --id=001 --trials=5 "$(cat data/config/lava-tasks)"
```