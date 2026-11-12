const path = require('path');
const { ROOT_PATH } = require('./webpack.constants');

/* the alias map: not executed, read statically */
const alias = {
  '~': path.join(ROOT_PATH, 'app/js'),
  lodash: 'lodash-es',
  jquery$: 'jquery/dist/jquery.slim.js',
  images: path.join(ROOT_PATH, 'app/images'),
  generated: makePath('tmp/generated'),
};

Object.assign(alias, {
  vendor: path.join(ROOT_PATH, 'vendor/js'),
});

module.exports = { resolve: { alias } };
