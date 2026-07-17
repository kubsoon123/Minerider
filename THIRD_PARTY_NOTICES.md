# Third-Party Notices

MineRider vendors data files from other open-source projects for use by its
build-time protocol generator. This file lists that vendored material and its
upstream license, separate from MineRider's own `MIT OR Apache-2.0` license
(see `LICENSE-MIT` / `LICENSE-APACHE`).

## minecraft-data (PrismarineJS)

Path: `crates/minerider-codegen/vendor/minecraft-data/`

Source: https://github.com/PrismarineJS/minecraft-data

License: MIT, per the upstream repository.

Copyright (c) PrismarineJS and contributors.

Vendored files: `pc/1.21.4/protocol.json`, `pc/1.21.4/version.json`,
`pc/1.21.4/blocks.json`, `pc/1.21.4/blockCollisionShapes.json`. These are
data files (packet layouts, block/collision tables) consumed at build time by
`minerider-codegen` and by `scripts/generate_collision_data.py`; no upstream
source code is vendored or redistributed as part of MineRider's compiled
output beyond the generated Rust produced from this data.

Per the upstream project's own notice, some of this data was originally
extracted from wiki.vg / minecraft.wiki (formerly minecraft.gamepedia.com);
consult the upstream repository for the current state of that attribution if
you redistribute the vendored data files themselves (as opposed to MineRider's
generated Rust code).
