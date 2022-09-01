"use strict";
const config = require("conventional-changelog-conventionalcommits");

module.exports = config({
  types: [
    { type: "feat", scope: "cli", section: "Features" },
    { type: "fix", scope: "cli", section: "Bug Fixes" },
    { type: "chore", hidden: true },
    { type: "docs", hidden: true },
    { type: "style", hidden: true },
    { type: "refactor", hidden: true },
    { type: "perf", hidden: true },
    { type: "test", hidden: true },
  ],
});
