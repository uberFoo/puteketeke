use async_compat::Compat;
use futures_lite::future;
use puteketeke::Executor;

fn main() {
    use warp::Filter;

    let executor = Executor::new(1);

    let future = Compat::new(async {
        let hello = warp::path!("hello" / String).map(|name| format!("Hello, {}!", name));

        warp::serve(hello).run(([127, 0, 0, 1], 3030)).await;
    });

    println!("Listening on http://localhost:3030/hello/you");

    let task = executor.spawn_task(future).unwrap();
    future::block_on(task);
}
