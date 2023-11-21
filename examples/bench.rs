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
                            dev_urandom.read(&mut buffer).unwrap();
                            // });
                        }
                        // task::block_in_place(move || {
                        let mut dev_null = File::create("/dev/null").unwrap();
                        dev_null.write(&mut buffer).unwrap();
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
    let _ = future::block_on(task);
}
