// CSV both ways, RFC 4180, no dependencies and no DOM, so node can test it
// (node dev/csv_test.mjs). Used by the bulk import page.

export class CsvError extends Error {
  constructor(message, line) {
    super(message);
    this.line = line;
  }
}

// parseCsv(text) -> {records: string[][], lines: number[]}; lines[i] is the
// line of the file on which record i starts (1-based).
//
// Accepts a UTF-8 byte order mark, CRLF, LF or CR line ends, a final line break
// or none, quoted values with "" for a quote and with commas or line breaks
// inside. A blank line is a record with one empty value; callers decide whether
// to skip it. Throws CsvError, with the line, for a quote that is never closed,
// a quote mark in the middle of an unquoted value, or text after a closing quote.
export function parseCsv(input) {
  let text = String(input ?? "");
  if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);
  const records = [];
  const lines = [];
  let record = [];
  let field = "";
  let line = 1;          // the line being read
  let start = 1;         // the line the current record started on
  let fieldStart = true; // nothing read yet for this value
  let inQuotes = false;
  let closed = false;    // this value was quoted and its closing quote is read
  const n = text.length;

  const endField = () => {
    record.push(field);
    field = "";
    fieldStart = true;
    closed = false;
  };
  const endRecord = () => {
    endField();
    records.push(record);
    lines.push(start);
    record = [];
  };

  for (let i = 0; i < n;) {
    const c = text[i];
    if (inQuotes) {
      if (c === '"') {
        if (text[i + 1] === '"') { field += '"'; i += 2; continue; }
        inQuotes = false;
        closed = true;
        i++;
        continue;
      }
      if (c === "\r" || c === "\n") {
        const crlf = c === "\r" && text[i + 1] === "\n";
        field += crlf ? "\r\n" : c;
        i += crlf ? 2 : 1;
        line++;
        continue;
      }
      field += c;
      i++;
      continue;
    }
    if (c === ",") { endField(); i++; continue; }
    if (c === "\r" || c === "\n") {
      endRecord();
      i += c === "\r" && text[i + 1] === "\n" ? 2 : 1;
      line++;
      start = line;
      continue;
    }
    if (closed) {
      throw new CsvError(`Line ${line}: there is text after a closing quote mark. Put the whole value inside the quotes.`, line);
    }
    if (c === '"') {
      if (fieldStart) { inQuotes = true; fieldStart = false; i++; continue; }
      throw new CsvError(`Line ${line}: a quote mark in the middle of a value. Put the whole value in quotes, and write a quote mark inside it as two ("").`, line);
    }
    field += c;
    fieldStart = false;
    i++;
  }
  if (inQuotes) {
    throw new CsvError(`Line ${start}: a quoted value is never closed. Check for a missing quote mark.`, start);
  }
  // the last record, when the file does not end with a line break
  if (!(fieldStart && !closed && record.length === 0)) endRecord();
  return { records, lines };
}

// One value, quoted only when it has to be.
export function csvValue(v) {
  const s = v === null || v === undefined ? "" : String(v);
  return /[",\r\n]|^\s|\s$/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

// Records to CSV text, CRLF line ends as RFC 4180 has them, ending with one.
export function toCsv(records) {
  return records.map((r) => r.map(csvValue).join(",")).join("\r\n") + "\r\n";
}
