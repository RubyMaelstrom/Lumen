// Correctness/calibration fixture: warm Intl.NumberFormat instances across the
// packed decimal, currency, compact, and non-Latin digit tables.
const CASES = [
  ["decimal", "en-US", { style: "decimal", maximumFractionDigits: 2 }],
  ["currency", "de-DE", { style: "currency", currency: "EUR" }],
  ["compact", "en-US", { notation: "compact", compactDisplay: "short" }],
  ["nonlatin", "ar-EG-u-nu-arab", { style: "decimal", maximumFractionDigits: 2 }],
];
const VALUES = [0, 1.25, 999.5, 1234567.89, -0.125];
const ITERATIONS = 30000;
const formatters = CASES.map(([name, locale, options]) => [
  name,
  new Intl.NumberFormat(locale, options),
]);
let hash = 2166136261;
let count = 0;
let first = "";
let last = "";
const samples = [];
for (const [name, formatter] of formatters) {
  samples.push([name, formatter.format(1234567.89), formatter.resolvedOptions().numberingSystem]);
}
for (let iteration = 0; iteration < ITERATIONS; iteration++) {
  for (let index = 0; index < formatters.length; index++) {
    const [name, formatter] = formatters[index];
    const value = formatter.format(VALUES[(iteration + index) % VALUES.length]);
    if (count === 0) first = value;
    last = value;
    let sample = value.length;
    for (let i = 0; i < value.length; i++) {
      sample = Math.imul(hash ^ value.charCodeAt(i), 16777619);
      hash = sample;
    }
    hash = Math.imul(hash ^ name.length, 16777619);
    count++;
  }
}
console.log(JSON.stringify({ iterations: ITERATIONS, count, hash: hash >>> 0, first, last, samples }));
