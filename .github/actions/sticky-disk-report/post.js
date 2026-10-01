// Runs report.py and turns any failure of it into a warning: the report
// describes the job, it is not a check of it.
const { spawnSync } = require("child_process");
const path = require("path");

const result = spawnSync("python3", [path.join(__dirname, "report.py")], { stdio: "inherit" });
if (result.error || result.status !== 0) {
  const reason = result.error ? result.error.message : `exit code ${result.status}`;
  console.log(`::warning title=Sticky disk report::The report failed (${reason}); the job result does not depend on it.`);
}
