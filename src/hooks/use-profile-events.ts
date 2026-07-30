import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import i18n from "@/i18n";
import type { BrowserProfile, GroupWithCount } from "@/types";

interface UseProfileEventsReturn {
  profiles: BrowserProfile[];
  groups: GroupWithCount[];
  runningProfiles: Set<string>;
  leasedProfiles: Set<string>;
  isLoading: boolean;
  error: string | null;
  loadProfiles: () => Promise<void>;
  loadGroups: () => Promise<void>;
  loadLeasedProfiles: () => Promise<void>;
  clearError: () => void;
}

/**
 * Custom hook to manage profile-related state and listen for backend events.
 * This hook eliminates the need for manual UI refreshes by automatically
 * updating state when the backend emits profile change events.
 */
export function useProfileEvents(): UseProfileEventsReturn {
  const [profiles, setProfiles] = useState<BrowserProfile[]>([]);
  const [groups, setGroups] = useState<GroupWithCount[]>([]);
  const [runningProfiles, setRunningProfiles] = useState<Set<string>>(
    new Set(),
  );
  const [leasedProfiles, setLeasedProfiles] = useState<Set<string>>(new Set());
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Load profiles from backend
  const loadProfiles = useCallback(async () => {
    try {
      const profileList = await invoke<BrowserProfile[]>(
        "list_browser_profiles",
      );
      setProfiles(profileList);
      setError(null);
    } catch (err: unknown) {
      console.error("Failed to load profiles:", err);
      setError(
        i18n.t("errors.loadProfilesFailed", { error: JSON.stringify(err) }),
      );
    }
  }, []);

  const loadLeasedProfiles = useCallback(async () => {
    try {
      const profileIds = await invoke<string[]>(
        "list_automation_leased_profile_ids",
      );
      setLeasedProfiles(new Set(profileIds));
    } catch (err) {
      console.error("Failed to load automation profile leases:", err);
    }
  }, []);

  // Load groups from backend
  const loadGroups = useCallback(async () => {
    try {
      const groupsWithCounts = await invoke<GroupWithCount[]>(
        "get_groups_with_profile_counts",
      );
      setGroups(groupsWithCounts);
      setError(null);
    } catch (err) {
      console.error("Failed to load groups with counts:", err);
      setGroups([]);
    }
  }, []);

  // Clear error state
  const clearError = useCallback(() => {
    setError(null);
  }, []);

  // Initial load and event listeners setup
  useEffect(() => {
    let profilesUnlisten: (() => void) | undefined;
    let runningUnlisten: (() => void) | undefined;

    const setupListeners = async () => {
      try {
        // Initial load
        await Promise.all([loadProfiles(), loadGroups(), loadLeasedProfiles()]);

        // Listen for profile changes (create, delete, rename, update, etc.)
        profilesUnlisten = await listen("profiles-changed", () => {
          console.log(
            "Received profiles-changed event, reloading profiles and groups",
          );
          void loadProfiles();
          void loadGroups();
          void loadLeasedProfiles();
        });

        // Listen for profile running state changes
        runningUnlisten = await listen<{ id: string; is_running: boolean }>(
          "profile-running-changed",
          (event) => {
            const { id, is_running } = event.payload;
            setRunningProfiles((prev) => {
              const next = new Set(prev);
              if (is_running) {
                next.add(id);
              } else {
                next.delete(id);
              }
              return next;
            });
            void loadLeasedProfiles();
          },
        );

        console.log("Profile event listeners set up successfully");
      } catch (err) {
        console.error("Failed to setup profile event listeners:", err);
        setError(
          i18n.t("errors.setupProfileListenersFailed", {
            error: JSON.stringify(err),
          }),
        );
      } finally {
        setIsLoading(false);
      }
    };

    void setupListeners();

    // Cleanup listeners on unmount
    return () => {
      if (profilesUnlisten) profilesUnlisten();
      if (runningUnlisten) runningUnlisten();
    };
  }, [loadProfiles, loadGroups, loadLeasedProfiles]);

  // Sync profile running states periodically to ensure consistency
  useEffect(() => {
    const syncRunningStates = async () => {
      await loadLeasedProfiles();
      if (profiles.length === 0) return;

      try {
        const statusChecks = profiles.map(async (profile) => {
          try {
            const isRunning = await invoke<boolean>("check_browser_status", {
              profile,
            });
            return { id: profile.id, isRunning };
          } catch (error) {
            console.error(
              `Failed to check status for profile ${profile.name}:`,
              error,
            );
            return { id: profile.id, isRunning: false };
          }
        });

        const statuses = await Promise.all(statusChecks);

        setRunningProfiles((prev) => {
          const next = new Set(prev);
          let hasChanges = false;

          statuses.forEach(({ id, isRunning }) => {
            if (isRunning && !prev.has(id)) {
              next.add(id);
              hasChanges = true;
            } else if (!isRunning && prev.has(id)) {
              next.delete(id);
              hasChanges = true;
            }
          });

          return hasChanges ? next : prev;
        });
      } catch (error) {
        console.error("Failed to sync profile running states:", error);
      }
    };

    // Initial sync
    void syncRunningStates();

    // Sync every 30 seconds to catch any missed events
    const interval = setInterval(() => {
      void syncRunningStates();
    }, 30000);

    return () => {
      clearInterval(interval);
    };
  }, [loadLeasedProfiles, profiles]);

  return {
    profiles,
    groups,
    runningProfiles,
    leasedProfiles,
    isLoading,
    error,
    loadProfiles,
    loadGroups,
    loadLeasedProfiles,
    clearError,
  };
}
