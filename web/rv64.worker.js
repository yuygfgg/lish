import { RV64 } from "./rv64.js";

const INTERNAL_EVENTS = ["stop", "error", "diskError"];

let vm = null;

function serializeError(error) {
  return {
    message: String(error?.message ?? error),
    name: error?.name ?? "Error",
    stack: error?.stack,
  };
}

function state() {
  return {
    instructions: vm ? vm.instructions : 0n,
    running: vm?.running ?? false,
    jitMetrics: vm?.jitMetrics() ?? null,
  };
}

function postEvent(event, detail) {
  self.postMessage({
    type: "event",
    event,
    detail: event === "error" ? serializeError(detail) : detail,
  });
}

async function create(message) {
  globalThis.LISH_DIAGNOSTICS = message.diagnostics === true;
  const eventNames = new Set([...INTERNAL_EVENTS, ...(message.eventNames ?? [])]);
  const events = Object.fromEntries(
    [...eventNames].map((event) => [event, (detail) => postEvent(event, detail)]),
  );
  vm = await RV64.create({
    ...message.options,
    events,
    execution: { mode: "local" },
  });
  self.postMessage({ type: "created", state: state() });
}

async function call(message) {
  try {
    let value;
    if (message.method === "start") value = await vm.start();
    else if (message.method === "stop") value = await vm.stop();
    else if (message.method === "reset") value = await vm.reset();
    else if (message.method === "destroy") {
      value = await vm.destroy();
      vm = null;
    } else throw new Error(`unknown Worker method: ${message.method}`);
    self.postMessage({ id: message.id, type: "result", value, state: state() });
  } catch (error) {
    self.postMessage({
      id: message.id,
      type: "result",
      error: serializeError(error),
      state: state(),
    });
  }
}

self.onmessage = (event) => {
  const message = event.data;
  if (message?.type === "create") {
    create(message).catch((error) => {
      self.postMessage({ type: "create-error", error: serializeError(error) });
    });
  } else if (message?.type === "call") {
    call(message);
  } else if (message?.type === "console") {
    vm?.console.send(message.value);
  } else if (message?.type === "network-receive") {
    vm?.network.receive(message.value);
  } else if (message?.type === "state-request" && vm) {
    self.postMessage({ type: "state", state: state() });
  }
};
