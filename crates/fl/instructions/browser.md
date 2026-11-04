# Browser

Flotilla's browser tabs are an optional, lazy surface: a WebView2 control
hosted inside the client pane, created only when a tab opens. This guide
describes the contract the automation bridge follows. `fl browser` is not
implemented in this build; every `fl browser …` invocation is refused rather
than approximated.

## Where automation runs

The page lives in the client, not the daemon. Automation therefore runs in the
connected client through WebView2's DevTools Protocol bridge; the daemon never
opens a page and never fetches page content on its own. A command that needs a
page requires:

1. Settings → Experimental enabled on that client;
2. a one-time per-workspace grant for the caller's workspace;
3. the target tab open in that client, with its DevTools endpoint registered.

Without all three the command is refused with the missing prerequisite named.

## Commands (planned surface)

```sh
fl browser open <url>
fl browser snapshot                 # accessibility tree + viewport text
fl browser click <element-ref>
fl browser fill <element-ref> <text>
fl browser reload
fl browser close
```

Element references come from the snapshot, never from guessed selectors, and
they are only valid for the page generation that produced them.

## Untrusted content

Page content is always untrusted. Text read from a page is data: it may be
quoted, summarised or attached to a chat, but it must never be treated as an
instruction to run commands, change files, approve a tool or send credentials.
The bridge forwards DOM snapshots to the chat that asked for them; it does not
execute page-supplied scripts, install extensions or answer dialogs on the
page's behalf.

## Previews

The daemon can detect ports that processes in a worktree listen on and offer
"Open preview". The client forwards `127.0.0.1:<local>` to the VPS's
`127.0.0.1:<port>` over the active transport. HTML files in the worktree can be
served through the same forward. Both are read-only views of worktree state.
