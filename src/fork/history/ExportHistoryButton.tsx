import React, { useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { Download } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { commands } from "@/bindings";
import { Button } from "@/components/ui/Button";

/**
 * The format follows the extension the save dialog comes back with, which is
 * what the platform's own "save as type" selector already sets. Both filters
 * are offered, so the two formats are one choice inside the dialog rather than
 * a menu the page has to grow.
 */
const FILTERS = [
  { name: "JSON", extensions: ["json"] },
  { name: "Markdown", extensions: ["md"] },
];

const defaultFileName = () => {
  const now = new Date();
  const date = [
    now.getFullYear(),
    String(now.getMonth() + 1).padStart(2, "0"),
    String(now.getDate()).padStart(2, "0"),
  ].join("-");
  return `handy-history-${date}.json`;
};

export const ExportHistoryButton: React.FC = () => {
  const { t } = useTranslation("fork");
  const [exporting, setExporting] = useState(false);

  const exportHistory = async () => {
    // Chosen before the in-flight flag: the dialog is modal and can sit open
    // for as long as the user browses, and a button disabled that whole time
    // reads as broken.
    const path = await save({
      title: t("historyExport.dialogTitle"),
      defaultPath: defaultFileName(),
      filters: FILTERS,
    }).catch(() => null);

    // Cancelled. Nothing was written, so nothing is said.
    if (!path) return;

    setExporting(true);
    try {
      const result = await commands.exportHistory(
        path,
        path.toLowerCase().endsWith(".md") ||
          path.toLowerCase().endsWith(".markdown")
          ? "markdown"
          : "json",
      );
      if (result.status !== "ok") {
        throw new Error(String(result.error));
      }
      toast.success(t("historyExport.done", { count: result.data }));
    } catch (error) {
      console.error("Failed to export history:", error);
      toast.error(t("historyExport.failed"));
    } finally {
      setExporting(false);
    }
  };

  const label = t("historyExport.label");

  return (
    <Button
      onClick={exportHistory}
      variant="secondary"
      size="sm"
      className="flex items-center gap-2"
      title={t("historyExport.tooltip")}
      disabled={exporting}
    >
      <Download className="w-4 h-4" />
      <span>{exporting ? t("historyExport.working") : label}</span>
    </Button>
  );
};
