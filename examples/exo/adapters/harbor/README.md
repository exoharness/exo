# Harbor Adapter

This adapter exists to allow the Harbor framework to wake up and await Exo
working on some task. The Harbor-side code lives in [`eval/harbor`](../../../../eval/harbor).

Transport is a local unix socket, default `~/.exo/harbor.sock`, one JSON line
per message. Each exchange is one request from Harbor and one blocking wait for
Exo's reply:

| Request               | Sent by                         | Response             |
| --------------------- | ------------------------------- | -------------------- |
| `task_started`        | `ExoAgent.run`                  | `task_complete`      |
| `verification_result` | `ExoSessionPlugin` on trial end | `feedback_processed` |

Exo thinks and takes actions until eventually responding with the
`send_adapter_message` tool, echoing the `target` from the inbound message.

This adapter is started when a job is started, and is used for all trials in
the job.

Note: the container for the conversation is _borrowed_. The harbor-side code
handles (a) creating the conversation for the task, and (b) attaching the container
that it starts.
