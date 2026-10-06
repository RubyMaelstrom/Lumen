// Warm-instance CLDR month/era lookup workload. Set ITERATIONS after one local calibration.
const ITERATIONS = 100;
const LOCALES = [
  "en", "en-IN", "de", "fr", "ja", "zh", "zh-Hant", "ar",
  "ru", "hi", "sv", "pt-PT"
];
const CALENDARS = [
  "gregory", "buddhist", "chinese", "coptic", "dangi", "ethioaa", "ethiopic",
  "hebrew", "indian", "islamic", "islamic-civil", "islamic-tbla",
  "islamic-umalqura", "japanese", "persian", "roc"
];
const formatters = [];
for (const locale of LOCALES) {
  for (const calendar of CALENDARS) {
    formatters.push(new Intl.DateTimeFormat(`${locale}-u-ca-${calendar}`, {
      year: "numeric", era: "long", month: "long", day: "numeric", timeZone: "UTC"
    }));
  }
}
const commonEra = new Date("2024-06-15T12:34:56.000Z");
const beforeCommonEra = new Date(0);
beforeCommonEra.setUTCFullYear(0, 2, 21);
beforeCommonEra.setUTCHours(12, 0, 0, 0);
const dates = [commonEra, beforeCommonEra];
let hash = 2166136261;
let first = "";
let last = "";
let count = 0;
for (let iteration = 0; iteration < ITERATIONS; iteration++) {
  for (let index = 0; index < formatters.length; index++) {
    for (let dateIndex = 0; dateIndex < dates.length; dateIndex++) {
      const value = formatters[index].format(dates[(iteration + index + dateIndex) & 1]);
      if (count === 0) first = value;
      last = value;
      let sample = value.length;
      if (value.length !== 0) {
        sample ^= value.charCodeAt(0);
        sample ^= value.charCodeAt(value.length - 1);
        sample ^= value.charCodeAt((value.length / 2) | 0);
      }
      hash = Math.imul(hash ^ sample, 16777619);
      count++;
    }
  }
}
console.log(JSON.stringify({ iterations: ITERATIONS, formatterCount: formatters.length, count, hash: hash >>> 0, first, last }));
