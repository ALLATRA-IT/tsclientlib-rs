use std::net::SocketAddr;

use anyhow::Result;
use futures::prelude::*;
use slog::info;
use structopt::StructOpt;
use tokio::time::{self, Duration};

mod utils;
use crate::utils::*;

#[derive(StructOpt, Clone, Debug)]
#[structopt(author, about)]
struct Args {
	/// The address of the server to connect to
	#[structopt(short = "a", long, default_value = "127.0.0.1:9987")]
	address: SocketAddr,
	/// The listening address of the client
	#[structopt(long, default_value = "0.0.0.0:0")]
	local_address: SocketAddr,
	/// Print the content of all packets
	///
	/// 0. Print nothing
	/// 1. Print command string
	/// 2. Print packets
	/// 3. Print udp packets
	#[structopt(short = "v", long, parse(from_occurrences))]
	verbose: u8,
	/// How many connections
	#[structopt()]
	count: usize,
}

#[tokio::main]
async fn main() -> Result<()> { real_main().await }

async fn real_main() -> Result<()> {
	// Parse command line options
	let args = Args::from_args();
	let logger = create_logger();

	stream::iter(0..args.count)
		.for_each_concurrent(None, |_| {
			let args = args.clone();
			let logger = logger.clone();
			tokio::spawn(async move {
				let mut con =
					create_client(args.local_address, args.address, logger.clone(), args.verbose)
						.await
						.unwrap();

				// Connect
				connect(&mut con).await.unwrap();
				info!(logger, "Connected");

				// Wait some time
				tokio::select! {
					_ = &mut time::delay_for(Duration::from_secs(2)) => {}
					_ = con.wait_disconnect() => {
						panic!("Disconnected");
					}
				};
				info!(logger, "Waited");

				// Disconnect
				let _ = disconnect(&mut con).await;
				info!(logger, "Disconnected");
			})
			.map(|_| ())
		})
		.await;

	Ok(())
}
