// Correctness/calibration fixture: repeated NumberFormat construction and
// resolved option reads, covering decimal, currency, compact, and non-Latin digits.
const CASES = [
  ["decimal", "en-US", { style: "decimal", maximumFractionDigits: 2 }, 1234567.89],
  ["currency", "de-DE", { style: "currency", currency: "EUR" }, 1234.5],
  ["compact", "en-US", { notation: "compact", compactDisplay: "short" }, 1234567.89],
  ["nonlatin", "ar-EG-u-nu-arab", { style: "decimal", maximumFractionDigits: 2 }, 1234567.89],
];
const ITERATIONS = 10000;
let hash = 2166136261;
let count = 0;
let first = "";
let last = "";
const samples = [];
for (const [name, locale, options, value] of CASES) {
  const formatter = new Intl.NumberFormat(locale, options);
  samples.push([name, formatter.format(value), formatter.resolvedOptions().numberingSystem]);
}
for (let iteration = 0; iteration < ITERATIONS; iteration++) {
  for (let index = 0; index < CASES.length; index++) {
    const [name, locale, options, number] = CASES[index];
    const formatter = new Intl.NumberFormat(locale, options);
    const value = formatter.format(number + (iteration % 3));
    const resolved = formatter.resolvedOptions();
    if (count === 0) first = value;
    last = value;
    hash = Math.imul(hash ^ value.length ^ name.length ^ resolved.locale.length, 16777619);
    for (let i = 0; i < value.length; i++) {
      hash = Math.imul(hash ^ value.charCodeAt(i), 16777619);
    }
    count++;
  }
}
console.log(JSON.stringify({ iterations: ITERATIONS, count, hash: hash >>> 0, first, last, samples }));
