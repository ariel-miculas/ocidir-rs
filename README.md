# ocidir

[![Crates.io][crates-badge]][crates-url]

[crates-badge]: https://img.shields.io/crates/v/ocidir.svg
[crates-url]: https://crates.io/crates/ocidir
[![docs.rs](https://docs.rs/ocidir/badge.svg)](https://docs.rs/ocidir)

# Read and write to OCI image layout directories

This library contains medium and low-level APIs for working with
[OCI images], which are basically a directory with blobs and JSON files
for metadata.

## Dependency on cap-std

This library makes use of [cap-std] to operate in a capability-oriented
fashion. In practice, the code in this project is well tested and would
not traverse outside its own path root. However, using capabilities
is a generally good idea when operating in the container ecosystem,
in particular when actively processing tar streams.

## Examples

To access an existing OCI directory:

```rust,no_run
# use ocidir::cap_std;
# use anyhow::{anyhow, Result};
# fn main() -> anyhow::Result<()> {
let d = cap_std::fs::Dir::open_ambient_dir("/path/to/ocidir", cap_std::ambient_authority())?;
let d = ocidir::OciDir::open(d)?;
println!("{:?}", d.read_index()?.ok_or_else(|| anyhow!("missing Image Index"))?);
# Ok(())
# }
```

[cap-std]: https://docs.rs/cap-std/
[OCI images]: https://github.com/opencontainers/image-spec
