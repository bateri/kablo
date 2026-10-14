# kablo

A transparent terminal session keeper. kablo holds the terminal (PTY) of a
shell so the programs in it outlive the connection that started them: close
the laptop, lose the network, restart the terminal app — the shell and
everything running in it keep going, and the next connection picks up where
the last one left off.

Unlike tmux or screen, kablo draws nothing. It does not put a terminal of its
own between the program and you; while a client is attached, the bytes the
program writes reach the client unchanged. Windows, tabs and splits belong to
the terminal app, not to kablo.

kablo is built to be driven by a terminal app — locally over a unix socket,
remotely over a plain `ssh host kablo attach` — and needs nothing from the
server beyond a login and the ability to run one command.

Status: early development, not yet released.

## License

kablo is free software: you can redistribute it and/or modify it under the
terms of the [GNU General Public License](LICENSE) as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version. It is distributed in the hope that it will be useful, but
WITHOUT ANY WARRANTY; see the license for details.

Copyright © 2026 Ömer Kala.
