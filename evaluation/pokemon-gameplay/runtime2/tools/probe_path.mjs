export default {
  name: "probe_path",
  description:
    "Press a movement/action sequence step-by-step and return a compact log of coordinates, facing, map, and battle state after each step. Useful for systematic room probing and detecting blocked tiles/warps.",
  parameters: {
    type: "object",
    properties: {
      buttons: {
        type: "array",
        items: {
          type: "string",
          enum: ["a", "b", "start", "select", "up", "down", "left", "right"],
        },
        minItems: 1,
        maxItems: 50,
      },
      hold_frames: { type: "number", minimum: 1, maximum: 120, default: 10 },
      wait_frames: { type: "number", minimum: 0, maximum: 600, default: 45 },
    },
    required: ["buttons"],
  },
  async execute(args, ctx) {
    const hold = args.hold_frames ?? 10;
    const wait = args.wait_frames ?? 45;
    const out = [];
    for (let i = 0; i < args.buttons.length; i++) {
      const btn = args.buttons[i];
      const res = await ctx.emulator.press([btn], hold, wait);
      const s = res.state || {};
      const line = {
        step: i + 1,
        button: btn,
        location: s.location,
        map: s.map_id,
        x: s.x,
        y: s.y,
        facing: s.facing,
        battle: s.battle,
        screen_hash: res.screen_hash,
      };
      out.push(line);
      ctx.log(JSON.stringify(line));
    }
    return { steps: out, final: out[out.length - 1] };
  },
};
