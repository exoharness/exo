export default {
  name: "route2_to_forest_approach",
  description:
    "From the standard Viridian north-exit landing on Route 2, follow the known safe overworld route into the gatehouse and out to the north-side forest-approach pocket, stopping early if a battle starts or an unexpected map appears.",
  parameters: {
    type: "object",
    properties: {
      hold_frames: { type: "number", default: 10 },
      wait_frames: { type: "number", default: 45 },
      stop_on_battle: { type: "boolean", default: true },
    },
    additionalProperties: false,
  },
  async execute(args, ctx) {
    const hold = args.hold_frames ?? 10;
    const wait = args.wait_frames ?? 45;
    const stopOnBattle = args.stop_on_battle ?? true;

    const segments = [
      {
        label: "route2_start_to_upper_left",
        buttons: [
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "left",
          "left",
          "left",
          "left",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
          "up",
        ],
      },
      {
        label: "enter_gatehouse",
        buttons: ["left", "left", "left", "left", "up"],
      },
      {
        label: "gatehouse_to_north_exit",
        buttons: ["up", "up", "right", "right", "up"],
      },
    ];

    const log = [];
    for (const seg of segments) {
      const res = await ctx.emulator.press(seg.buttons, hold, wait);
      const s = res.state || {};
      log.push({
        segment: seg.label,
        map: s.map_name || s.map || null,
        x: s.x ?? null,
        y: s.y ?? null,
        battle: s.battle_type || s.battle || null,
        screen_hash: res.screen_hash,
      });
      if (stopOnBattle && s.battle_type && s.battle_type !== "none") {
        ctx.log(`Stopped during ${seg.label}: battle started.`);
        return { ok: true, stopped: "battle", log, state: s };
      }
    }

    const finalState = log.length ? log[log.length - 1].state : undefined;
    return { ok: true, stopped: null, log };
  },
};
