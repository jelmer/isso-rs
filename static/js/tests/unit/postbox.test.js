/**
 * @jest-environment jsdom
 */

/* Keep the above exactly as-is!
 * https://jestjs.io/docs/configuration#testenvironment-string
 * https://jestjs.io/docs/configuration#testenvironmentoptions-object
 */

"use strict";

jest.mock("app/api");

test('Create Postbox', () => {
  // Set up our document body
  document.body.innerHTML =
    '<div id=isso-thread></div>' +
    // Note: `src` and `data-isso` need to be set,
    // else `api` fails to initialize!
    '<script src="http://isso.api/js/embed.min.js" data-isso="/"></script>';

  const isso = require("app/isso");
  const $ = require("app/dom");

  const config = require("app/config");
  const i18n = require("app/i18n");
  const svg = require("app/svg");

  const template = require("app/template");

  template.set("conf", config);
  template.set("i18n", i18n.translate);
  template.set("pluralize", i18n.pluralize);
  template.set("svg", svg);

  var isso_thread = $('#isso-thread');
  isso_thread.append('<div id="isso-root"></div>');
  isso_thread.append(new isso.Postbox(null));

  // Will create a `.snap` file in `./__snapshots__/`.
  // Don't forget to check in those files when changing anything!
  expect(isso_thread.innerHTML).toMatchSnapshot();
});

test('Postbox shows guard rejection message and clears it on next success', async () => {
  // jsdom doesn't implement scrollIntoView; isso.js calls it on submit.
  window.HTMLElement.prototype.scrollIntoView = jest.fn();

  document.body.innerHTML =
    '<div id=isso-thread></div>' +
    '<script src="http://isso.api/js/embed.min.js" data-isso="/"></script>';

  const isso = require("app/isso");
  const $ = require("app/dom");
  const api = require("app/api");
  const i18n = require("app/i18n");

  var isso_thread = $('#isso-thread');
  isso_thread.append('<div id="isso-root"></div>');
  var postbox = isso_thread.append(new isso.Postbox(null));

  $(".isso-textarea", postbox).value = "Lorem ipsum";
  var errorEl = $(".isso-postbox-error", postbox);
  var submit = $("[type=submit]", postbox);

  // `click()` runs the submit handler synchronously, which attaches its
  // `.then()` callbacks to the promise returned by `api.create()`. Callbacks
  // run in registration order, so once our own reaction on that same promise
  // fires, the handler's success/error callback has already completed.
  const settleLastCreate = () => {
    const results = api.create.mock.results;
    return results[results.length - 1].value.then(() => {}, () => {});
  };

  api.create.mockRejectedValueOnce({ status: 403, reason: "reply-to-self" });
  submit.obj.click();
  expect(api.create).toHaveBeenCalledTimes(1);
  await settleLastCreate();

  expect(errorEl.textContent).toBe(i18n.translate("guard-reply-to-self"));
  expect(errorEl.obj.hidden).toBe(false);

  api.create.mockResolvedValueOnce({
    id: 1, parent: null, text: "Lorem ipsum", created: Date.now() / 1000, hash: "deadbeef",
  });
  submit.obj.click();
  expect(api.create).toHaveBeenCalledTimes(2);
  await settleLastCreate();

  expect(errorEl.textContent).toBe("");
  expect(errorEl.obj.hidden).toBe(true);
});
