// Warm-instance DisplayNames lookup control. Instances are created before the
// lookup loop; generated CLDR tables account for the repeated `of` calls.
const ITERATIONS = 160;
const LOCALES = [
  "en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru",
  "ar", "sr", "th", "gv", "sl", "pl", "si", "ln", "sv", "hi"
];
const GROUPS = [
  ["language", ["fr", "en-GB", "en-Latn-GB", "qz"], ["long", "short"]],
  ["region", ["US", "GB", "419", "qz"], ["long", "short", "narrow"]],
  ["script", ["Latn", "Hans", "Cyrl", "Zzzz"], ["long", "short"]],
  ["currency", ["USD", "EUR", "JPY", "xyz"], ["long"]],
  ["calendar", ["gregory", "islamic-civil", "japanese", "ABC"], ["long"]],
  ["dateTimeField", ["month", "year", "timeZoneName", "hour"], ["long", "short", "narrow"]]
];

const cases = [];
for (let localeIndex = 0; localeIndex < LOCALES.length; localeIndex++) {
  const locale = LOCALES[localeIndex];
  for (let groupIndex = 0; groupIndex < GROUPS.length; groupIndex++) {
    const [type, codes, styles] = GROUPS[groupIndex];
    for (let styleIndex = 0; styleIndex < styles.length; styleIndex++) {
      const options = {
        type,
        style: styles[styleIndex],
        fallback: (localeIndex + groupIndex + styleIndex) % 2 ? "none" : "code"
      };
      if (type === "language")
        options.languageDisplay = (localeIndex + styleIndex) % 2 ? "standard" : "dialect";
      cases.push([new Intl.DisplayNames(locale, options), codes]);
    }
  }
}

let hash = 2166136261;
let count = 0;
let first = "";
let last = "";
for (let iteration = 0; iteration < ITERATIONS; iteration++) {
  for (let caseIndex = 0; caseIndex < cases.length; caseIndex++) {
    const [names, codes] = cases[caseIndex];
    for (let codeIndex = 0; codeIndex < codes.length; codeIndex++) {
      const value = names.of(codes[codeIndex]);
      const text = value === undefined ? "<undefined>" : value;
      if (count === 0) first = text;
      last = text;
      for (let index = 0; index < text.length; index++)
        hash = Math.imul(hash ^ text.charCodeAt(index), 16777619);
      hash = Math.imul(hash ^ 255, 16777619);
      count++;
    }
  }
}
console.log(JSON.stringify({iterations: ITERATIONS, localeCount: LOCALES.length,
  caseCount: cases.length, count, hash: hash >>> 0, first, last}));
