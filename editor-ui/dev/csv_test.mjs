// Unit tests for js/csv.js, with node's built-in test runner (no dependencies):
//
//   node --test dev/csv_test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { parseCsv, toCsv, csvValue, CsvError } from "../js/csv.js";

const records = (text) => parseCsv(text).records;

test("plain values, with and without a final line break", () => {
  assert.deepEqual(records("a,b,c\n1,2,3\n"), [["a", "b", "c"], ["1", "2", "3"]]);
  assert.deepEqual(records("a,b,c\n1,2,3"), [["a", "b", "c"], ["1", "2", "3"]]);
});

test("CRLF, LF and lone CR line ends", () => {
  assert.deepEqual(records("a,b\r\n1,2\r\n"), [["a", "b"], ["1", "2"]]);
  assert.deepEqual(records("a,b\r1,2\r"), [["a", "b"], ["1", "2"]]);
  assert.deepEqual(records("a,b\r\n1,2\n3,4"), [["a", "b"], ["1", "2"], ["3", "4"]]);
});

test("an empty file has no records", () => {
  assert.deepEqual(parseCsv(""), { records: [], lines: [] });
  assert.deepEqual(records("\ufeff"), []);
});

test("a byte order mark is dropped", () => {
  assert.deepEqual(records("\ufeffstop_id,name\nx,y\n"), [["stop_id", "name"], ["x", "y"]]);
});

test("empty values, a trailing comma and a quoted empty value", () => {
  assert.deepEqual(records(",a,,\n"), [["", "a", "", ""]]);
  assert.deepEqual(records('""\n'), [[""]]);
  assert.deepEqual(records('a,""'), [["a", ""]]);
});

test("quoted values keep commas, quotes and line breaks", () => {
  assert.deepEqual(records('name,note\n"Adyar, Gandhi Nagar","He said ""stop"""\n'),
    [["name", "note"], ["Adyar, Gandhi Nagar", 'He said "stop"']]);
  assert.deepEqual(records('a,"line one\nline two",b\n'), [["a", "line one\nline two", "b"]]);
  assert.deepEqual(records('a,"line one\r\nline two"\r\n'), [["a", "line one\r\nline two"]]);
});

test("each record knows the line it starts on", () => {
  const { records: r, lines } = parseCsv('h1,h2\nx,"two\nlines"\ny,z\n\nlast,row');
  assert.deepEqual(r, [["h1", "h2"], ["x", "two\nlines"], ["y", "z"], [""], ["last", "row"]]);
  assert.deepEqual(lines, [1, 2, 4, 5, 6]);
});

test("a blank line is a record with one empty value", () => {
  assert.deepEqual(records("a\n\nb\n"), [["a"], [""], ["b"]]);
});

test("UTF-8 text such as Tamil names is kept as it is", () => {
  assert.deepEqual(records("name,regional\nAdyar,அடையாறு\n"), [["name", "regional"], ["Adyar", "அடையாறு"]]);
});

test("spaces are part of an unquoted value", () => {
  assert.deepEqual(records(" a , b \n"), [[" a ", " b "]]);
});

test("a quote that is never closed is an error on the line it opened", () => {
  assert.throws(() => parseCsv('a,b\n1,"open\n2,3\n'), (e) => e instanceof CsvError && e.line === 2 && /never closed/.test(e.message));
});

test("a quote mark inside an unquoted value is an error", () => {
  assert.throws(() => parseCsv('a,b\n12" pipe,x\n'), (e) => e instanceof CsvError && e.line === 2 && /middle of a value/.test(e.message));
});

test("text after a closing quote is an error", () => {
  assert.throws(() => parseCsv('"abc"def,x\n'), (e) => e instanceof CsvError && e.line === 1 && /after a closing quote/.test(e.message));
});

test("csvValue quotes only when it must", () => {
  assert.equal(csvValue("plain"), "plain");
  assert.equal(csvValue("a,b"), '"a,b"');
  assert.equal(csvValue('say "hi"'), '"say ""hi"""');
  assert.equal(csvValue("two\nlines"), '"two\nlines"');
  assert.equal(csvValue(" padded"), '" padded"');
  assert.equal(csvValue(null), "");
  assert.equal(csvValue(13.0827), "13.0827");
});

test("toCsv and parseCsv round-trip", () => {
  const rows = [["route_id", "stage_name"], ["570X", "KELAMBAKKAM, OMR"], ["21G", 'The "Broadway"'], ["x", "multi\r\nline"], ["", ""]];
  const text = toCsv(rows);
  assert.ok(text.endsWith("\r\n"));
  assert.deepEqual(records(text), rows);
});
