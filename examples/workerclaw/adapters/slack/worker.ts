import process from "node:process";

import {
  adapterConfig,
  optionalStringField,
  writeWorkerEvent,
} from "../protocol";

const config = adapterConfig();
const defaultChannelId = optionalStringField(config, "defaultChannelId");

writeWorkerEvent({
  type: "lifecycle",
  name: "placeholder",
  metadata: {
    message:
      "Slack adapter is not implemented yet. Outbound messaging via send_adapter_message will fail until the worker is completed.",
    defaultChannelId: defaultChannelId ?? null,
  },
});

writeWorkerEvent({
  type: "connected",
  subject: defaultChannelId ?? "slack-placeholder",
  metadata: { implemented: false },
});

process.stderr.write(
  "[slack-adapter] Placeholder worker running (no Slack connection).\n",
);

// Keep process alive so the adapter runtime considers the worker supervised.
await new Promise<void>(() => {});
