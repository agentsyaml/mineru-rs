const { canonicalStem, validatePdfOptions } = require('../index.js');

function assert(cond, msg) {
  if (!cond) throw new Error(msg || 'assertion failed');
}

// canonicalStem sanitizes non-portable characters.
assert(canonicalStem('a bad/pdf') === 'a_bad_pdf', 'stem sanitize');

// Empty stem defaults to "document".
assert(canonicalStem('') === 'document', 'empty default');

// Non-ASCII stem is rejected.
try {
  canonicalStem('bad\u4e2d');
  throw new Error('expected throw for non-ASCII stem');
} catch (e) {
  if (!(e instanceof Error)) throw e;
}

// Default options validate.
assert(validatePdfOptions(0, null, true, true, true) === true, 'default options');

// Inverted range is rejected.
try {
  validatePdfOptions(5, 2, true, true, true);
  throw new Error('expected throw for inverted range');
} catch (e) {
  if (!(e instanceof Error)) throw e;
}

console.log('node smoke: all assertions passed');