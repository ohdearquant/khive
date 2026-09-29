// Commands in this registry only navigate, change the visible view, or read
// from the current surface. Mutating review decisions and imports stay out.
export const APP_COMMAND_COPY = {
  showcase: "Repository showcase",
  showcaseDetail: "Open repository analyses.",
  review: "KG review",
  reviewDetail: "Open the local review workspace.",
  reviewStateRouteBlocked: "Unavailable while this review has unsaved local state.",
  reviewReportCurrentView: "Already viewing the imported report.",
  reviewViewDetail: "Switch review view.",
  reviewNavigation: "Review navigation",
  reviewCommands: "Review commands",
  reviewSearch: "Search review commands",
  reviewResults: "Review command results",
  reviewPlaceholder: "Jump to a view or action",
  noMatches: "No command matches this query.",
  copyCli: "Copy CLI",
  copyCliDetail: "Copy the command for this review.",
  download: "Download review bundle",
  downloadDetail: "Save the current review bundle locally.",
  downloadReport: "Download review report",
  downloadReportDetail: "Save the current CLI review report locally.",
} as const;

export type NavigationCommand = Readonly<{
  id: "navigation:showcase" | "navigation:review";
  kind: "navigation";
  label: string;
  detail: string;
  href: "/" | "/review";
}>;

export const APP_NAVIGATION_COMMANDS: readonly NavigationCommand[] = [
  {
    id: "navigation:showcase",
    kind: "navigation",
    label: APP_COMMAND_COPY.showcase,
    detail: APP_COMMAND_COPY.showcaseDetail,
    href: "/",
  },
  {
    id: "navigation:review",
    kind: "navigation",
    label: APP_COMMAND_COPY.review,
    detail: APP_COMMAND_COPY.reviewDetail,
    href: "/review",
  },
];
