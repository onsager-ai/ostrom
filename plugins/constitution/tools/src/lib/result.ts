export type Status = "OK" | "WARN" | "FAIL" | "DEFER";

export interface CheckResult {
  status: Status;
  name: string;
  detail: string;
  remedy: string;
}

function sanitize(value: string): string {
  return value.replace(/[\r\n]/g, " ").replaceAll("|", "/");
}

export function formatResult(result: CheckResult): string {
  return [
    result.status,
    result.name,
    sanitize(result.detail),
    sanitize(result.remedy),
  ].join("|");
}
