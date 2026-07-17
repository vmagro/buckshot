function green(s) {
  return "\x1b[32m" + s + "\x1b[0m";
}

function live_reload_color() {
  return "blue";
}

module.exports = { green, live_reload_color };
