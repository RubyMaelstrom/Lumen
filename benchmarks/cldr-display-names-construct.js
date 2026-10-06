// DisplayNames construction control. It intentionally does not call `of`.
const ITERATIONS = 100;
const LOCALES = [
  "en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru",
  "ar", "sr", "th", "gv", "sl", "pl", "si", "ln", "sv", "hi"
];
const GROUPS = [
  ["language", ["long", "short"]],
  ["region", ["long", "short", "narrow"]],
  ["script", ["long", "short"]],
  ["currency", ["long"]],
  ["calendar", ["long"]],
  ["dateTimeField", ["long", "short", "narrow"]]
];

let hash = 2166136261;
let count = 0;
for (let iteration = 0; iteration < ITERATIONS; iteration++) {
  for (let localeIndex = 0; localeIndex < LOCALES.length; localeIndex++) {
    const locale = LOCALES[localeIndex];
    for (let groupIndex = 0; groupIndex < GROUPS.length; groupIndex++) {
      const [type, styles] = GROUPS[groupIndex];
      for (let styleIndex = 0; styleIndex < styles.length; styleIndex++) {
        const options = {
          type,
          style: styles[styleIndex],
          fallback: (localeIndex + groupIndex + styleIndex) % 2 ? "none" : "code"
        };
        if (type === "language")
          options.languageDisplay = (localeIndex + styleIndex) % 2 ? "standard" : "dialect";
        const result = new Intl.DisplayNames(locale, options).resolvedOptions();
        hash = Math.imul(hash ^ result.locale.length, 16777619);
        hash = Math.imul(hash ^ result.type.length, 16777619);
        hash = Math.imul(hash ^ result.style.length, 16777619);
        hash = Math.imul(hash ^ result.fallback.length, 16777619);
        if (result.languageDisplay)
          hash = Math.imul(hash ^ result.languageDisplay.length, 16777619);
        count++;
      }
    }
  }
}
console.log(JSON.stringify({iterations: ITERATIONS, localeCount: LOCALES.length,
  count, hash: hash >>> 0}));
