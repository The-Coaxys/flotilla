# Layout

A Flotilla layout belongs to one worktree (or to the home workspace). It is a
tree of views; each view has ordered tabs; each tab has a tree of panes; each
pane owns one terminal. Layout indexes are never identities: commands take
stable ids (`ViewId`, `TabId`).

## Reading the layout

```sh
fl layout views --json
fl tab title --tab <tab-id> "Build"
fl tab split --view <view-id> --direction down --columns 120 --rows 32
fl tab split-view --view <view-id> --direction right
```

`layout views` prints every view, its split ratio, and each tab's panes and
terminals. `--json` is the stable machine form; render it from ids, not from
positions.

## Changing it

```sh
fl layout set <view-id> --ratio 3500
fl layout close <view-id>
```

- `set` selects a view and, with `--ratio`, sets the split that contains it.
  Ratios are basis points in 1000..9000; `100` is refused.
- `close` cannot close the final view. Closing a tab releases its terminal
  intent; the daemon reaps it.
- Every mutation commits against the revision the command read. A concurrent
  change produces a revision conflict and the CLI retries once against a fresh
  revision; a second conflict is reported instead of overwriting.

## Composing operations safely

```sh
fl layout compose --dry-run --file ops.json --json
```

`compose` reads a JSON array of layout operations, validates every one against
the current layout, and reports the resulting revision, the terminals it would
spawn and any error — without applying anything. This build never applies a
composed batch; use `tab`/`layout` mutations for real changes and `compose`
to check a generated plan.

## Notes

- The daemon spawns a terminal for every pane a layout operation creates; a
  tab without a terminal is not representable.
- Layout changes are published as `LayoutChanged` events carrying the scope and
  revision, and a client that reconnects refetches the whole layout.
- `layout capabilities`, `insert`, `move`, `save`, `apply`, `list` and `run`
  are not implemented in this build; they are refused with a clear message
  rather than approximated.
