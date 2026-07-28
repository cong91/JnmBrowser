import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

async function assertListenerBeforeInvoke(path, callbackName, commandName) {
  const source = await readFile(new URL(path, import.meta.url), "utf8");
  const callbackStart = source.indexOf(`const ${callbackName} = useCallback(`);
  const listener = source.indexOf("await ensureListener();", callbackStart);
  const invoke = source.indexOf(
    `invoke<string>("${commandName}"`,
    callbackStart,
  );

  assert.notEqual(callbackStart, -1, `${callbackName} callback is missing`);
  assert.notEqual(listener, -1, `${callbackName} does not await its listener`);
  assert.notEqual(invoke, -1, `${callbackName} command invoke is missing`);
  assert.ok(
    listener < invoke,
    `${callbackName} invokes before its listener is ready`,
  );
}

async function assertStrictModeListenerGeneration(path) {
  const source = await readFile(new URL(path, import.meta.url), "utf8");

  assert.match(
    source,
    /const generation = \+\+listenerGenerationRef\.current;/,
  );
  assert.match(source, /generation !== listenerGenerationRef\.current/);
  assert.match(source, /generation === listenerGenerationRef\.current/);
  assert.match(source, /listenerGenerationRef\.current \+= 1;/);
}

test("registration listener is ready before task start", async () => {
  await assertListenerBeforeInvoke(
    "../hooks/use-registration-events.ts",
    "startRegistration",
    "start_auto_registration",
  );
});

test("login listener is ready before task start", async () => {
  await assertListenerBeforeInvoke(
    "../hooks/use-login-events.ts",
    "startLogin",
    "start_auto_login",
  );
});

test("registration listener generation rejects stale StrictMode setup", async () => {
  await assertStrictModeListenerGeneration(
    "../hooks/use-registration-events.ts",
  );
});

test("login listener generation rejects stale StrictMode setup", async () => {
  await assertStrictModeListenerGeneration("../hooks/use-login-events.ts");
});
