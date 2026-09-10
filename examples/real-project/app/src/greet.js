'use strict';

function greet(name, platform = process.platform) {
  return `Hello, ${name}! Built on ${platform}.`;
}

module.exports = { greet };
