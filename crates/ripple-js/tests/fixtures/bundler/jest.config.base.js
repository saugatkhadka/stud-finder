const path = require('path');

module.exports = (dir, options = {}) => {
  const moduleNameMapper = {
    '^~(/.*)$': '<rootDir>/app/js$1',
    '^~(/.*)\\?raw$': '<rootDir>/app/js$1',
    // CE first; the EE block below adds the EE directory for the same prefix
    '^ee_else_ce(/.*)$': '<rootDir>/app/js$1',
    '^ee_component(/.*)$': '<rootDir>/app/js/empty.js',
    '^jquery$': '<rootDir>/node_modules/jquery/dist/jquery.slim.js',
    '^jest/(.*)$': '<rootDir>/spec/frontend/$1',
    '^dyn(/.*)$': computeDir('dyn'),
    '^gone(/.*)$': '<rootDir>/no/such/dir$1',
  };
  if (isEE) {
    const rootDirEE = '<rootDir>/ee/app/js$1';
    Object.assign(moduleNameMapper, {
      '^ee(/.*)$': rootDirEE,
      '^ee_else_ce(/.*)$': rootDirEE,
    });
  }
  return { moduleNameMapper };
};
