# libp2p-bistream

*Maintenance status: Experimental, work in progress*

A very simple network behavior for [libp2p](https://github.com/libp2p/rust-libp2p) that exposes binary duplex streams.

It works with the usual libp2p stack of transports and muxers, and allows application-level code to work directly on any number of bidirectional streams between peers.

See the [example](examples/minimal/src/main.rs) for a usage example.

Optionally, a *managed* mode is available, where the streams opened and accepted through this behavior may be managed outside of the main libp2p event loop. See [examples/managed](examples/managed/src/main.rs) for an example.

## License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br />

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>
