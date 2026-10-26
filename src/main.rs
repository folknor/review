/// The keeper is dispatched before the tokio runtime exists. It is a
/// synchronous, single-threaded process that owns every wait status in its
/// tree (see `review::keeper`), so it must never share a process with tokio's
/// child reaper or with `review`'s signal supervisor.
fn main() -> anyhow::Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(review::keeper::ARG.as_ref()) {
        std::process::exit(review::keeper::main());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(review::app::run())
}
