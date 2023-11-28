//! From https://gist.github.com/miquels/8576d1394d3b26c6811f4fc1e7886a1c
//! and https://www.reddit.com/r/rust/comments/lg0a7b/benchmarking_tokio_tasks_and_goroutines/
use std::{
    fs::File,
    io::{Read, Write},
    time::Instant,
};

use futures_lite::future;
use puteketeke::{AsyncTask, Executor};

fn main() {
    let executor = Executor::new(1);
    // use tokio::task::{self, JoinHandle};

    async fn compute(executor: &Executor) {
        let handles: Vec<AsyncTask<_>> = (0..1000)
            .map(|_| {
                executor
                    .spawn_task(async move {
                        let mut buffer = [0; 10];
                        {
                            // task::block_in_place(move || {
                            let mut dev_urandom = File::open("/dev/urandom").unwrap();
                            dev_urandom.read_exact(&mut buffer).unwrap();
                            // });
                        }
                        // task::block_in_place(move || {
                        let mut dev_null = File::create("/dev/null").unwrap();
                        dev_null.write_all(&buffer).unwrap();
                        // });
                    })
                    .unwrap()
            })
            .collect();
        for handle in handles {
            handle.await;
        }
    }

    let inner = executor.clone();
    let main = async move {
        // warmup
        compute(&inner).await;

        let before = Instant::now();
        for _ in 0usize..1000 {
            compute(&inner).await;
        }
        let elapsed = before.elapsed();
        println!(
            "{:?} total, {:?} avg per iteration",
            elapsed,
            elapsed / 1000
        );
    };

    let task = executor.spawn_task(main).unwrap();
    future::block_on(task);
}
