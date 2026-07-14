"use client";

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuDownload, LuPlay, LuTrash2 } from "react-icons/lu";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";
import type { BrowserProfile, ExportedRecipe, RecordingSummary } from "@/types";
import { RippleButton } from "./ui/ripple";

interface RecorderDialogProps {
  isOpen: boolean;
  onClose: () => void;
  allProfiles: BrowserProfile[];
  runningProfiles: Set<string>;
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const secs = Math.round(ms / 1000);
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  const rem = secs % 60;
  return `${mins}m ${rem}s`;
}

export function RecorderDialog({
  isOpen,
  onClose,
  allProfiles,
  runningProfiles,
}: RecorderDialogProps) {
  const { t } = useTranslation();
  const [recordings, setRecordings] = useState<RecordingSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);

  const loadRecordings = useCallback(async () => {
    setLoading(true);
    try {
      const data = await invoke<RecordingSummary[]>("list_recordings");
      setRecordings(data);
    } catch (err) {
      console.error("Failed to list recordings:", err);
      showErrorToast(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (isOpen) {
      void loadRecordings();
    }
  }, [isOpen, loadRecordings]);

  const handleDelete = useCallback(
    async (id: string) => {
      setBusyId(id);
      try {
        await invoke<boolean>("delete_recording", { id });
        setRecordings((prev) => prev.filter((r) => r.id !== id));
        showSuccessToast(t("recorder.deleted"));
      } catch (err) {
        console.error("Failed to delete recording:", err);
        showErrorToast(err instanceof Error ? err.message : String(err));
      } finally {
        setBusyId(null);
      }
    },
    [t],
  );

  const handleExport = useCallback(
    async (id: string) => {
      setBusyId(id);
      try {
        const recipe = await invoke<ExportedRecipe>(
          "export_recording_as_recipe",
          {
            id,
          },
        );
        const text = JSON.stringify(recipe, null, 2);
        await navigator.clipboard.writeText(text);
        showSuccessToast(t("recorder.exported"));
      } catch (err) {
        console.error("Failed to export recording:", err);
        showErrorToast(err instanceof Error ? err.message : String(err));
      } finally {
        setBusyId(null);
      }
    },
    [t],
  );

  const handleReplay = useCallback(
    async (recording: RecordingSummary) => {
      // Prefer replaying on a running profile that matches the original browser.
      const candidates = allProfiles.filter(
        (p) =>
          runningProfiles.has(p.id) &&
          (p.browser === recording.browser ||
            (recording.browser === "chromium" &&
              p.browser.includes("chromium"))),
      );
      const target =
        candidates.find((p) => p.id === recording.profile_id) ?? candidates[0];
      if (!target) {
        showErrorToast(t("recorder.noRunningProfile"));
        return;
      }
      setBusyId(recording.id);
      try {
        await invoke("replay_recording", {
          id: recording.id,
          profileId: target.id,
        });
        showSuccessToast(t("recorder.replayStarted"));
      } catch (err) {
        console.error("Failed to replay recording:", err);
        showErrorToast(err instanceof Error ? err.message : String(err));
      } finally {
        setBusyId(null);
      }
    },
    [allProfiles, runningProfiles, t],
  );

  const handleOpenChange = useCallback(
    (open: boolean) => {
      if (!open) onClose();
    },
    [onClose],
  );

  return (
    <Dialog open={isOpen} onOpenChange={handleOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t("recorder.dialogTitle")}</DialogTitle>
          <DialogDescription>
            {t("recorder.dialogDescription")}
          </DialogDescription>
        </DialogHeader>

        <ScrollArea className="max-h-[420px] pr-2">
          {loading ? (
            <div className="py-8 text-center text-muted-foreground text-sm">
              {t("recorder.loading")}
            </div>
          ) : recordings.length === 0 ? (
            <div className="py-8 text-center text-muted-foreground text-sm">
              {t("recorder.empty")}
            </div>
          ) : (
            <ul className="space-y-2">
              {recordings.map((rec) => {
                const busy = busyId === rec.id;
                return (
                  <li
                    key={rec.id}
                    className="flex items-start justify-between gap-3 rounded-md border border-border p-3"
                  >
                    <div className="min-w-0 flex-1 space-y-1">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-medium text-sm truncate">
                          {rec.profile_name}
                        </span>
                        <Badge variant="secondary" className="text-xs">
                          {rec.browser}
                        </Badge>
                        <span className="text-xs text-muted-foreground">
                          {t("recorder.eventCount", { count: rec.event_count })}
                        </span>
                        <span className="text-xs text-muted-foreground">
                          {formatDuration(rec.duration_ms)}
                        </span>
                      </div>
                      <div className="text-xs text-muted-foreground truncate">
                        {rec.start_url || rec.id}
                      </div>
                      <div className="text-xs text-muted-foreground">
                        {rec.created_at}
                      </div>
                    </div>
                    <div className="flex shrink-0 items-center gap-1">
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <span>
                            <RippleButton
                              variant="ghost"
                              size="icon"
                              disabled={busy}
                              onClick={() => void handleReplay(rec)}
                              className="h-8 w-8"
                            >
                              <LuPlay className="h-4 w-4" />
                            </RippleButton>
                          </span>
                        </TooltipTrigger>
                        <TooltipContent>{t("recorder.replay")}</TooltipContent>
                      </Tooltip>
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <span>
                            <RippleButton
                              variant="ghost"
                              size="icon"
                              disabled={busy}
                              onClick={() => void handleExport(rec.id)}
                              className="h-8 w-8"
                            >
                              <LuDownload className="h-4 w-4" />
                            </RippleButton>
                          </span>
                        </TooltipTrigger>
                        <TooltipContent>
                          {t("recorder.exportAsRecipe")}
                        </TooltipContent>
                      </Tooltip>
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <span>
                            <RippleButton
                              variant="ghost"
                              size="icon"
                              disabled={busy}
                              onClick={() => void handleDelete(rec.id)}
                              className="h-8 w-8 text-destructive"
                            >
                              <LuTrash2 className="h-4 w-4" />
                            </RippleButton>
                          </span>
                        </TooltipTrigger>
                        <TooltipContent>{t("recorder.delete")}</TooltipContent>
                      </Tooltip>
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </ScrollArea>
      </DialogContent>
    </Dialog>
  );
}
