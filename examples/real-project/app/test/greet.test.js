'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { greet } = require('../src/greet');

test('greets by name', () => {
  assert.strictEqual(greet('minact', 'linux'), 'Hello, minact! Built on linux.');
});

test('reports the platform it runs on', () => {
  assert.match(greet('ci'), /Built on (darwin|win32|linux)\./);
});
