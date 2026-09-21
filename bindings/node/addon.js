'use strict'

try {
  const native = require('./index.js')
  module.exports = native
  module.exports.chromium = native.Chromium
  module.exports.default = module.exports
} catch (err) {
  module.exports = {
    chromium: {
      launch: async () => {
        throw new Error('build the native addon: npm run build')
      },
    },
  }
  module.exports.default = module.exports
}
