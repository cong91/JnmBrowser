import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import type { RecorderSessionInfo } from "@/types";

/**
 * Hook to track active action-recording sessions and look up per-profile state.
 */
export function useRecorderSessions() {
  const [sessions, setSessions] = useState<RecorderSessionInfo[]>([]);

  const loadSessions = useCallback(async () => {
    try {
      const data = await invoke<RecorderSessionInfo[]>("get_recorder_sessions");
      setSessions(data);
    } catch (err) {
      console.error("Failed to load recorder sessions:", err);
    }
  }, []);

  useEffect(() => {
    let changedUnlisten: (() => void) | undefined;
    let endedUnlisten: (() => void) | undefined;
    let sessionsChangedUnlisten: (() => void) | undefined;

    const setup = async () => {
      await loadSessions();

      changedUnlisten = await listen<RecorderSessionInfo>(
        "recorder-session-changed",
        (event) => {
          setSessions((prev) => {
            const idx = prev.findIndex((s) => s.id === event.payload.id);
            if (idx >= 0) {
              const next = [...prev];
              next[idx] = event.payload;
              return next;
            }
            return [...prev, event.payload];
          });
        },
      );

      endedUnlisten = await listen<string>(
        "recorder-session-ended",
        (event) => {
          setSessions((prev) => prev.filter((s) => s.id !== event.payload));
        },
      );

      sessionsChangedUnlisten = await listen(
        "recorder-sessions-changed",
        () => {
          void loadSessions();
        },
      );
    };

    void setup();

    return () => {
      changedUnlisten?.();
      endedUnlisten?.();
      sessionsChangedUnlisten?.();
    };
  }, [loadSessions]);

  const getProfileRecorderInfo = useCallback(
    (profileId: string): { session: RecorderSessionInfo } | undefined => {
      const session = sessions.find((s) => s.profile_id === profileId);
      if (!session) return undefined;
      return { session };
    },
    [sessions],
  );

  return { sessions, getProfileRecorderInfo, loadSessions };
}
