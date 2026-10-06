// Formatter-construction control: creates locale/calendar formatters but does not format dates.
const ITERATIONS = 300;
const LOCALES = [
  "en", "en-IN", "de", "fr", "ja", "zh", "zh-Hant", "ar",
  "ru", "hi", "sv", "pt-PT"
];
const CALENDARS = [
  "gregory", "buddhist", "chinese", "coptic", "dangi", "ethioaa", "ethiopic",
  "hebrew", "indian", "islamic", "islamic-civil", "islamic-tbla",
  "islamic-umalqura", "japanese", "persian", "roc"
];
let hash = 2166136261;
let count = 0;
for (let iteration = 0; iteration < ITERATIONS; iteration++) {
  for (const locale of LOCALES) {
    for (const calendar of CALENDARS) {
      const formatter = new Intl.DateTimeFormat(`${locale}-u-ca-${calendar}`, {
        year: "numeric", era: "long", month: "long", day: "numeric", timeZone: "UTC"
      });
      const options = formatter.resolvedOptions();
      hash = Math.imul(hash ^ options.calendar.length ^ options.locale.length, 16777619);
      count++;
    }
  }
}
console.log(JSON.stringify({ iterations: ITERATIONS, count, hash: hash >>> 0 }));
