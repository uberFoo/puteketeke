![Build Status](https://github.com/uberFoo/puteketeke/workflows/rust/badge.svg)
[![codecov](https://codecov.io/gh/uberFoo/puteketeke/graph/badge.svg?token=eCmOPZzxX5)](https://codecov.io/gh/uberFoo/puteketeke)
![Lines of Code](https://tokei.rs/b1/github/uberfoo/puteketeke)

# Pūteketeke: Rust Async Runtime

This crate takes [smol](https://github.com/smol-rs/smol) and builds upon it to provide a more intuitive and complete runtime for async Rust programs.
This is what you might end up with if you started with smol, added a threadpool and made the API easier to use.

Here's a hello world program.

```rust
// Create an executor with four threads.
use puteketeke::{AsyncTask, Executor};
use futures_lite::future;

let executor = Executor::new(4);

let task = executor
    .create_task(async { println!("Hello world") })
    .unwrap();

// Note that we need to start the task, otherwise it will never run.
executor.start_task(&task);
future::block_on(task);
 ```

 Check out the [documentation](https://docs.rs/puteketeke/latest/puteketeke/index.html) for usage information.

## Purpose — or; do we need another runtime?

For me the answer to that question is an obvious yes.
The need for `p8e` arose out of my need to add async support to my programming language, [dwarf](https://github.com/uberFoo/dwarf).
I already had the language, and I just wanted to "bolt-on" async -- without making everything async.
It turned out to be a lot of trial and error, and learning a lot more about async Rust than I wanted to know.
Given that, I thought that other folks out there might benefit.
So I calved off the code into a crate.

Primarily this crate is intended to be used by those that wish to have some async in their code, and don't want to commit fully.
This crate does not require wrapping main in 'async fn`, and it really makes working with futures pretty simple.
The features of note are:

- an RAII executor, backed by a threadpool, that cleans up after itself when dropped
- a task that starts paused, and are freely passed around
- the ability to create isolated workers upon which tasks are executed
- an ergonomic and simple API
- parallel timers

Practically speaking, the greatest need that I had in dwarf, besides an executor, was the ability to enqueue a paused task.
It's not really too difficult to explain why either.
Consider this dwarf code:

```rust
async fn main() -> Future<()> {
    let future = async {
        print(42);
    };
    print("The answer to life the universe and everything is: ");
    future.await;
}
```

As you might guess, this prints "The answer to life the universe and everything is: 42".

The interpreter is processing statements and expressions.
When the block expression:

```rust
async {
    print(42);
}
```

is processed, we don't want the future to start executing yet, otherwise the sentence would be wrong.
So we need to hold on to that future and only start it when it is awaited, after the print expression.
Furthermore, we need to be able to pass that future around the interpreter as we process the next statement.

## License

Pūteketeke is distributed under the terms of both the MIT license and the Apache License (Version 2.0).

 See [LICENSE-APACHE](LICENSE-APACHE), [LICENSE-MIT](LICENSE-MIT) for details.
