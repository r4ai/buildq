const fs = require('node:fs');
const path = require('node:path');

module.exports = async ({ core }) => {
  const report = JSON.parse(fs.readFileSync('target/llvm-cov/summary.json', 'utf8')).data[0];
  const format = value => value.count === 0 ? 'N/A' : `${value.percent.toFixed(2)}%`;
  const rows = [['Lines', 'lines'], ['Functions', 'functions'], ['Regions', 'regions']]
    .map(([label, key]) => {
      const value = report.totals[key];
      return `| ${label} | **${format(value)}** | ${value.covered} / ${value.count} |`;
    });
  const files = report.files.filter(file => file.summary.lines.count > 0)
    .sort((a, b) => a.summary.lines.percent - b.summary.lines.percent)
    .slice(0, 10)
    .map(file => {
      const name = path.relative(process.env.GITHUB_WORKSPACE, file.filename)
        .replaceAll('\\', '/').replace(/[|`<>\r\n]/g, '_');
      return `| ${name} | ${format(file.summary.lines)} |`;
    });
  const body = [
    '<!-- buildq-coverage -->',
    '## Rust coverage · Linux',
    '',
    `Commit: ${process.env.COVERAGE_SHA}`,
    '',
    '| Metric | Coverage | Covered / Total |',
    '| :--- | ---: | ---: |',
    ...rows,
    '',
    `[Download HTML + LCOV reports](${process.env.COVERAGE_ARTIFACT_URL}) · [CI run](${process.env.COVERAGE_RUN_URL})`,
    '',
    '<details><summary>10 files with the lowest line coverage</summary>',
    '',
    '| File | Line coverage |',
    '| :--- | ---: |',
    ...files,
    '',
    '</details>',
    '',
    'Rust unit and integration tests only. Ignored E2E tests and macOS/Windows-specific code are excluded. No minimum percentage gate.',
  ].join('\n');
  await core.summary.addRaw(body).write();
  core.setOutput('body', body);
};
