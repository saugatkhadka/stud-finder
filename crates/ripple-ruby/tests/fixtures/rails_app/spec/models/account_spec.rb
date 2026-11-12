require "rails_helper"

RSpec.describe Account do
  it { expect(Status::VISIBILITY).to be_present }
end
