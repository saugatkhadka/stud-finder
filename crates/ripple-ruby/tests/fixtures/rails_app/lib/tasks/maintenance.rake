namespace :maintenance do
  task run: :environment do
    Shared
    Reopened
    TagParser.new
  end
end
