'use strict';

// A stand-in for a real build: writes a versioned bundle into dist/.
const fs = require('node:fs');
const path = require('node:path');
const pkg = require('./package.json');
const { greet } = require('./src/greet');

const dist = path.join(__dirname, 'dist');
fs.mkdirSync(dist, { recursive: true });
fs.writeFileSync(
  path.join(dist, 'bundle.js'),
  `// ${pkg.name} ${pkg.version} (${process.platform} node ${process.version})\n` +
    fs.readFileSync(path.join(__dirname, 'src', 'greet.js'), 'utf8'),
);
fs.writeFileSync(path.join(dist, 'BUILD.txt'), greet('minact') + '\n');
console.log(`built ${pkg.name} ${pkg.version} into ${dist}`);
